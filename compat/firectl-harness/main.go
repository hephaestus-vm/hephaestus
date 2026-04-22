// firectl-harness — drive hephaestus-firecracker over the same
// firecracker-go-sdk client that firectl/Kata/firecracker-containerd use.
//
// firectl itself doesn't build on darwin (its SDK transitively imports
// containernetworking/plugins/pkg/ns, which has no darwin files), and it
// hard-fork/exec's a `firecracker` binary it manages itself. Both blockers
// are bypassed here by importing only the swagger-generated client + models
// from the SDK and pointing them at an existing UNIX socket — the rest of
// the SDK (machine.go, network.go) never gets pulled in.
//
// What this is for: catching wire-shape drift in hephaestus-fc-api that
// curl-based smoke tests miss. The SDK's strict deserialization will trip
// on missing required fields, name typos, type mismatches, etc. — the same
// failure modes a real Go orchestrator would hit.
//
// Usage:
//   ./firectl-harness \
//     -sock /tmp/hephaestus-firecracker.socket \
//     -kernel /path/to/vmlinux \
//     -rootfs /path/to/alpine.ext4 \
//     [-skip-boot]   # config-only run, no InstanceStart
package main

import (
	"context"
	"flag"
	"fmt"
	"net"
	"net/http"
	"os"
	"path/filepath"
	"time"

	httptransport "github.com/go-openapi/runtime/client"
	"github.com/go-openapi/strfmt"

	"github.com/firecracker-microvm/firecracker-go-sdk/client/models"
	"github.com/firecracker-microvm/firecracker-go-sdk/client/operations"
)

func main() {
	var (
		sock     = flag.String("sock", "/tmp/hephaestus-firecracker.socket", "path to hephaestus-firecracker UNIX socket")
		kernel   = flag.String("kernel", "", "path to guest kernel image (vmlinux)")
		rootfs   = flag.String("rootfs", "", "path to ext4 rootfs")
		bootArgs = flag.String("boot-args", "console=ttyS0 reboot=k panic=1 nomodule quiet loglevel=3", "kernel cmdline")
		logFile  = flag.String("log", "", "if set, PUT /logger with this path before boot")
		skipBoot = flag.Bool("skip-boot", false, "configure only, do not InstanceStart")
		pause    = flag.Bool("pause", false, "after boot, exercise PATCH /vm Paused/Resumed")
		vcpu     = flag.Int64("vcpu", 1, "vcpu count for PUT /machine-config")
		mem      = flag.Int64("mem", 256, "initial memory MiB for PUT /machine-config")
		memPatch = flag.Int64("mem-patch", 512, "memory MiB for the PATCH /machine-config bump")
		snapSave = flag.String("snapshot-save", "", "after boot+pause, PUT /snapshot/create writing state to this path (and a stub at .mem)")
		snapLoad = flag.String("snapshot-load", "", "instead of InstanceStart, PUT /snapshot/load from this state path with resume_vm=true")
	)
	flag.Parse()

	if *kernel == "" || *rootfs == "" {
		fmt.Fprintln(os.Stderr, "-kernel and -rootfs are required")
		flag.Usage()
		os.Exit(2)
	}
	for label, p := range map[string]string{"kernel": *kernel, "rootfs": *rootfs} {
		if _, err := os.Stat(p); err != nil {
			fmt.Fprintf(os.Stderr, "%s: %v\n", label, err)
			os.Exit(2)
		}
	}

	abs := func(p string) string {
		out, _ := filepath.Abs(p)
		return out
	}

	client := newUnixClient(*sock)
	ops := newOperationsClient(client)

	h := &harness{ops: ops}

	h.run("GET /", func() error {
		out, err := ops.DescribeInstance(operations.NewDescribeInstanceParams())
		if err != nil {
			return err
		}
		return assertInstanceInfo(out.GetPayload())
	})

	h.run("GET /machine-config (default)", func() error {
		out, err := ops.GetMachineConfiguration(operations.NewGetMachineConfigurationParams())
		if err != nil {
			return err
		}
		mc := out.GetPayload()
		fmt.Printf("    vcpu=%d mem=%dMiB\n", deref64(mc.VcpuCount), deref64(mc.MemSizeMib))
		return nil
	})

	if *logFile != "" {
		h.run("PUT /logger", func() error {
			lp := abs(*logFile)
			_, err := ops.PutLogger(operations.NewPutLoggerParams().WithBody(&models.Logger{
				LogPath: &lp,
			}))
			return err
		})
		h.run("PUT /metrics", func() error {
			mp := abs(*logFile + ".metrics")
			_, err := ops.PutMetrics(operations.NewPutMetricsParams().WithBody(&models.Metrics{
				MetricsPath: &mp,
			}))
			return err
		})
	}

	h.run("PUT /machine-config", func() error {
		_, err := ops.PutMachineConfiguration(operations.NewPutMachineConfigurationParams().WithBody(&models.MachineConfiguration{
			VcpuCount:  vcpu,
			MemSizeMib: mem,
		}))
		return err
	})

	h.run("PATCH /machine-config (mem bump)", func() error {
		_, err := ops.PatchMachineConfiguration(operations.NewPatchMachineConfigurationParams().WithBody(&models.MachineConfiguration{
			MemSizeMib: memPatch,
		}))
		return err
	})

	h.run("PUT /boot-source", func() error {
		kp := abs(*kernel)
		_, err := ops.PutGuestBootSource(operations.NewPutGuestBootSourceParams().WithBody(&models.BootSource{
			KernelImagePath: &kp,
			BootArgs:        *bootArgs,
		}))
		return err
	})

	h.run("PUT /drives/rootfs", func() error {
		var (
			id  = "rootfs"
			ph  = abs(*rootfs)
			ro  = true
			rd  = true
		)
		_, err := ops.PutGuestDriveByID(operations.NewPutGuestDriveByIDParams().
			WithDriveID(id).
			WithBody(&models.Drive{
				DriveID:      &id,
				PathOnHost:   &ph,
				IsReadOnly:   &ro,
				IsRootDevice: &rd,
			}))
		return err
	})

	h.run("PATCH /drives/rootfs (path swap pre-boot)", func() error {
		// Swap path back to itself — exercises the PATCH wire shape
		// without changing the actual file (so boot still works).
		id := "rootfs"
		ph := abs(*rootfs)
		_, err := ops.PatchGuestDriveByID(operations.NewPatchGuestDriveByIDParams().
			WithDriveID(id).
			WithBody(&models.PartialDrive{
				DriveID:    &id,
				PathOnHost: ph,
			}))
		return err
	})

	if *skipBoot {
		fmt.Println("\n-skip-boot set; configuration-only run complete")
		h.summary()
		return
	}

	if *snapLoad != "" {
		// Snapshot-load mode: replaces InstanceStart entirely. The VM is
		// expected to be in `Running` state immediately after the call
		// (resume_vm=true), so the assertions below match the cold-boot
		// path's expectations without re-running InstanceStart.
		h.run("PUT /snapshot/load", func() error {
			snap := abs(*snapLoad)
			memStub := snap + ".mem"
			_, err := ops.LoadSnapshot(operations.NewLoadSnapshotParams().WithBody(&models.SnapshotLoadParams{
				SnapshotPath: &snap,
				MemFilePath:  &memStub,
				ResumeVM:     true,
			}))
			return err
		})
	} else {
		h.run("PUT /actions InstanceStart", func() error {
			action := "InstanceStart"
			_, err := ops.CreateSyncAction(operations.NewCreateSyncActionParams().WithInfo(&models.InstanceActionInfo{
				ActionType: &action,
			}))
			return err
		})
	}

	h.run("GET / (post-boot, expect Running)", func() error {
		out, err := ops.DescribeInstance(operations.NewDescribeInstanceParams())
		if err != nil {
			return err
		}
		state := derefStr(out.GetPayload().State)
		fmt.Printf("    state=%q\n", state)
		if state != "Running" {
			return fmt.Errorf("expected Running, got %q", state)
		}
		return nil
	})

	if *pause || *snapSave != "" {
		time.Sleep(200 * time.Millisecond)
		h.run("PATCH /vm Paused", func() error {
			s := "Paused"
			_, err := ops.PatchVM(operations.NewPatchVMParams().WithBody(&models.VM{State: &s}))
			return err
		})
		h.run("GET / (expect Paused)", func() error {
			out, err := ops.DescribeInstance(operations.NewDescribeInstanceParams())
			if err != nil {
				return err
			}
			state := derefStr(out.GetPayload().State)
			fmt.Printf("    state=%q\n", state)
			if state != "Paused" {
				return fmt.Errorf("expected Paused, got %q", state)
			}
			return nil
		})
		if *snapSave != "" {
			h.run("PUT /snapshot/create", func() error {
				snap := abs(*snapSave)
				mem := snap + ".mem"
				snapType := "Full"
				_, err := ops.CreateSnapshot(operations.NewCreateSnapshotParams().WithBody(&models.SnapshotCreateParams{
					SnapshotPath: &snap,
					MemFilePath:  &mem,
					SnapshotType: snapType,
				}))
				if err != nil {
					return err
				}
				if _, statErr := os.Stat(snap); statErr != nil {
					return fmt.Errorf("snapshot blob missing at %s: %w", snap, statErr)
				}
				if _, statErr := os.Stat(mem); statErr != nil {
					return fmt.Errorf("mem stub missing at %s: %w", mem, statErr)
				}
				fmt.Printf("    blob=%s mem-stub=%s\n", snap, mem)
				return nil
			})
		} else {
			h.run("PATCH /vm Resumed", func() error {
				s := "Resumed"
				_, err := ops.PatchVM(operations.NewPatchVMParams().WithBody(&models.VM{State: &s}))
				return err
			})
		}
	}

	h.summary()
}

type harness struct {
	ops    *operations.Client
	passed int
	failed int
}

func (h *harness) run(name string, fn func() error) {
	fmt.Printf("==> %s\n", name)
	if err := fn(); err != nil {
		fmt.Printf("    FAIL: %v\n", err)
		h.failed++
		return
	}
	fmt.Println("    OK")
	h.passed++
}

func (h *harness) summary() {
	fmt.Printf("\n%d passed, %d failed\n", h.passed, h.failed)
	if h.failed > 0 {
		os.Exit(1)
	}
}

// newUnixClient returns an http.Client whose transport dials the given
// UNIX socket regardless of the URL host. The SDK's go-openapi runtime
// uses URL-style addressing (host=localhost), so we override Dial.
func newUnixClient(sock string) *http.Client {
	return &http.Client{
		Transport: &http.Transport{
			DialContext: func(ctx context.Context, _, _ string) (net.Conn, error) {
				var d net.Dialer
				return d.DialContext(ctx, "unix", sock)
			},
		},
		Timeout: 10 * time.Second,
	}
}

func newOperationsClient(c *http.Client) *operations.Client {
	transport := httptransport.NewWithClient("localhost", "/", []string{"http"}, c)
	return operations.New(transport, strfmt.Default)
}

func assertInstanceInfo(info *models.InstanceInfo) error {
	if info == nil {
		return fmt.Errorf("nil InstanceInfo")
	}
	for label, ptr := range map[string]*string{
		"app_name":    info.AppName,
		"id":          info.ID,
		"state":       info.State,
		"vmm_version": info.VmmVersion,
	} {
		if ptr == nil {
			return fmt.Errorf("required field %q missing from response", label)
		}
	}
	fmt.Printf("    id=%q state=%q app=%q version=%q\n",
		*info.ID, *info.State, *info.AppName, *info.VmmVersion)
	return nil
}

func derefStr(p *string) string {
	if p == nil {
		return "<nil>"
	}
	return *p
}

func deref64(p *int64) int64 {
	if p == nil {
		return -1
	}
	return *p
}
