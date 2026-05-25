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
//
//	./firectl-harness \
//	  -sock /tmp/hephaestus-firecracker.socket \
//	  -kernel /path/to/vmlinux \
//	  -rootfs /path/to/alpine.ext4 \
//	  [-skip-boot]   # config-only run, no InstanceStart
package main

import (
	"bytes"
	"context"
	"encoding/json"
	"flag"
	"fmt"
	"io"
	"net"
	"net/http"
	"os"
	"path/filepath"
	"strings"
	"time"

	httptransport "github.com/go-openapi/runtime/client"
	"github.com/go-openapi/strfmt"

	"github.com/firecracker-microvm/firecracker-go-sdk/client/models"
	"github.com/firecracker-microvm/firecracker-go-sdk/client/operations"
)

func main() {
	var (
		sock      = flag.String("sock", "/tmp/hephaestus-firecracker.socket", "path to hephaestus-firecracker UNIX socket")
		kernel    = flag.String("kernel", "", "path to guest kernel image (vmlinux)")
		rootfs    = flag.String("rootfs", "", "path to ext4 rootfs")
		bootArgs  = flag.String("boot-args", "console=ttyS0 reboot=k panic=1 nomodule quiet loglevel=3", "kernel cmdline")
		logFile   = flag.String("log", "", "if set, PUT /logger with this path before boot")
		skipBoot  = flag.Bool("skip-boot", false, "configure only, do not InstanceStart")
		pause     = flag.Bool("pause", false, "after boot, exercise PATCH /vm Paused/Resumed")
		skipVsock = flag.Bool("skip-vsock", false, "do not configure PUT /vsock (for stock-init pools without a socket device)")
		vcpu      = flag.Int64("vcpu", 1, "vcpu count for PUT /machine-config")
		mem       = flag.Int64("mem", 256, "initial memory MiB for PUT /machine-config")
		memPatch  = flag.Int64("mem-patch", 512, "memory MiB for the PATCH /machine-config bump")
		snapSave  = flag.String("snapshot-save", "", "after boot+pause, PUT /snapshot/create writing state to this path (and a stub at .mem)")
		snapLoad  = flag.String("snapshot-load", "", "instead of InstanceStart, PUT /snapshot/load from this state path with resume_vm=true")
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

	h := &harness{ops: ops, http: client}

	h.run("GET /", func() error {
		out, err := ops.DescribeInstance(operations.NewDescribeInstanceParams())
		if err != nil {
			return err
		}
		return assertInstanceInfo(out.GetPayload())
	})

	h.run("GET /version", func() error {
		out, err := ops.GetFirecrackerVersion(operations.NewGetFirecrackerVersionParams())
		if err != nil {
			return err
		}
		if out.GetPayload() == nil || out.GetPayload().FirecrackerVersion == nil {
			return fmt.Errorf("required field firecracker_version missing")
		}
		fmt.Printf("    firecracker_version=%q\n", *out.GetPayload().FirecrackerVersion)
		return nil
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
			level := models.LoggerLevelDebug
			showLevel := true
			showOrigin := true
			_, err := ops.PutLogger(operations.NewPutLoggerParams().WithBody(&models.Logger{
				LogPath:       &lp,
				Level:         &level,
				ShowLevel:     &showLevel,
				ShowLogOrigin: &showOrigin,
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

	h.run("PUT /mmds/config rejects non-link-local IPv4", func() error {
		return expectErr("PUT /mmds/config non-link-local IPv4", func() error {
			version := "V2"
			ipv4 := "10.0.0.2"
			_, err := ops.PutMmdsConfig(operations.NewPutMmdsConfigParams().WithBody(&models.MmdsConfig{
				IPV4Address:       &ipv4,
				NetworkInterfaces: []string{},
				Version:           &version,
			}))
			return err
		})
	})

	h.run("PUT /mmds/config", func() error {
		version := "V2"
		ipv4 := "169.254.169.254"
		_, err := ops.PutMmdsConfig(operations.NewPutMmdsConfigParams().WithBody(&models.MmdsConfig{
			IPV4Address:       &ipv4,
			NetworkInterfaces: []string{},
			Version:           &version,
		}))
		return err
	})

	h.run("PUT/PATCH/GET /mmds", func() error {
		_, err := ops.PutMmds(operations.NewPutMmdsParams().WithBody(map[string]interface{}{
			"latest": map[string]interface{}{
				"meta-data": map[string]interface{}{"instance-id": "i-hephaestus"},
			},
		}))
		if err != nil {
			return err
		}
		_, err = ops.PatchMmds(operations.NewPatchMmdsParams().WithBody(map[string]interface{}{
			"latest": map[string]interface{}{
				"user-data": "hello",
			},
		}))
		if err != nil {
			return err
		}
		out, err := ops.GetMmds(operations.NewGetMmdsParams())
		if err != nil {
			return err
		}
		fmt.Printf("    mmds=%v\n", out.GetPayload())
		return nil
	})

	if !*skipVsock {
		h.run("PUT /vsock", func() error {
			cid := int64(3)
			uds := filepath.Join(os.TempDir(), "hephaestus-vsock.sock")
			_, err := ops.PutGuestVsock(operations.NewPutGuestVsockParams().WithBody(&models.Vsock{GuestCid: &cid, UdsPath: &uds}))
			return err
		})
	}

	h.run("PUT /actions FlushMetrics", func() error {
		action := "FlushMetrics"
		_, err := ops.CreateSyncAction(operations.NewCreateSyncActionParams().WithInfo(&models.InstanceActionInfo{
			ActionType: &action,
		}))
		return err
	})

	h.run("unsupported device endpoints return errors", func() error {
		if err := expectErr("PUT /balloon", func() error {
			amount := int64(64)
			deflate := true
			_, err := ops.PutBalloon(operations.NewPutBalloonParams().WithBody(&models.Balloon{AmountMib: &amount, DeflateOnOom: &deflate}))
			return err
		}); err != nil {
			return err
		}
		if err := expectErr("PATCH /balloon", func() error {
			amount := int64(32)
			_, err := ops.PatchBalloon(operations.NewPatchBalloonParams().WithBody(&models.BalloonUpdate{AmountMib: &amount}))
			return err
		}); err != nil {
			return err
		}
		if err := expectErr("GET /balloon", func() error {
			_, err := ops.DescribeBalloonConfig(operations.NewDescribeBalloonConfigParams())
			return err
		}); err != nil {
			return err
		}
		if err := expectErr("GET /balloon/statistics", func() error {
			_, err := ops.DescribeBalloonStats(operations.NewDescribeBalloonStatsParams())
			return err
		}); err != nil {
			return err
		}
		if err := expectErr("PATCH /balloon/statistics", func() error {
			interval := int64(1)
			_, err := ops.PatchBalloonStatsInterval(operations.NewPatchBalloonStatsIntervalParams().WithBody(&models.BalloonStatsUpdate{StatsPollingIntervals: &interval}))
			return err
		}); err != nil {
			return err
		}
		checks := []struct{ name, method, path, body string }{
			{"PATCH /balloon/hinting/start", http.MethodPatch, "/balloon/hinting/start", `{}`},
			{"GET /balloon/hinting/status", http.MethodGet, "/balloon/hinting/status", ``},
			{"PATCH /balloon/hinting/stop", http.MethodPatch, "/balloon/hinting/stop", `{}`},
			{"PUT /entropy", http.MethodPut, "/entropy", `{"rate_limiter":null}`},
			{"PUT /cpu-config", http.MethodPut, "/cpu-config", `{}`},
			{"PATCH /cpu-config", http.MethodPatch, "/cpu-config", `{}`},
			{"PUT /pmem/pmem0", http.MethodPut, "/pmem/pmem0", `{"pmem_id":"pmem0","host_path":"/tmp/pmem","size":1048576}`},
			{"PUT /serial", http.MethodPut, "/serial", `{"file_path":"/tmp/serial.log"}`},
			{"GET /hotplug/memory", http.MethodGet, "/hotplug/memory", ``},
			{"PUT /hotplug/memory", http.MethodPut, "/hotplug/memory", `{"size_mib":128}`},
			{"PATCH /hotplug/memory", http.MethodPatch, "/hotplug/memory", `{"desired_size_mib":256}`},
			{"GET /vm/config", http.MethodGet, "/vm/config", ``},
		}
		for _, check := range checks {
			if err := h.expectHTTPError(check.name, check.method, check.path, check.body); err != nil {
				return err
			}
		}
		return nil
	})

	h.run("PUT /machine-config rejects cpu_template", func() error {
		return expectErr("PUT /machine-config cpu_template", func() error {
			t := models.CPUTemplateT2
			_, err := ops.PutMachineConfiguration(operations.NewPutMachineConfigurationParams().WithBody(&models.MachineConfiguration{
				VcpuCount:   vcpu,
				MemSizeMib:  mem,
				CPUTemplate: models.CPUTemplate(t),
			}))
			return err
		})
	})

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
			id = "rootfs"
			ph = abs(*rootfs)
			ro = true
			rd = true
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
		if *logFile != "" {
			h.run("logger output contains Firecracker-style records", func() error {
				return assertLogContains(abs(*logFile), []string{"[", ":DEBUG:", "request_id=", "api_server::request"})
			})
			h.run("metrics output contains Firecracker-style JSON", func() error {
				return assertMetricsJSON(abs(*logFile + ".metrics"))
			})
		}
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

	if *logFile != "" {
		h.run("logger output contains Firecracker-style records", func() error {
			return assertLogContains(abs(*logFile), []string{"[", ":DEBUG:", "request_id=", "api_server::request"})
		})
		h.run("metrics output contains Firecracker-style JSON", func() error {
			return assertMetricsJSON(abs(*logFile + ".metrics"))
		})
	}

	h.summary()
}

type harness struct {
	ops    *operations.Client
	http   *http.Client
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

func expectErr(name string, fn func() error) error {
	if err := fn(); err == nil {
		return fmt.Errorf("%s unexpectedly succeeded", name)
	}
	fmt.Printf("    %s -> error\n", name)
	return nil
}

func (h *harness) expectHTTPError(name, method, path, body string) error {
	req, err := http.NewRequest(method, "http://localhost"+path, bytes.NewBufferString(body))
	if err != nil {
		return err
	}
	req.Header.Set("content-type", "application/json")
	resp, err := h.http.Do(req)
	if err != nil {
		return err
	}
	defer resp.Body.Close()
	payload, _ := io.ReadAll(resp.Body)
	if resp.StatusCode < 400 {
		return fmt.Errorf("%s unexpectedly returned %d: %s", name, resp.StatusCode, string(payload))
	}
	fmt.Printf("    %s -> %d\n", name, resp.StatusCode)
	return nil
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

func assertLogContains(path string, needles []string) error {
	payload, err := os.ReadFile(path)
	if err != nil {
		return err
	}
	text := string(payload)
	for _, needle := range needles {
		if !strings.Contains(text, needle) {
			return fmt.Errorf("log %s missing %q; content: %s", path, needle, text)
		}
	}
	fmt.Printf("    log=%s bytes=%d\n", path, len(payload))
	return nil
}

func assertMetricsJSON(path string) error {
	payload, err := os.ReadFile(path)
	if err != nil {
		return err
	}
	lines := strings.Split(strings.TrimSpace(string(payload)), "\n")
	if len(lines) == 0 || lines[0] == "" {
		return fmt.Errorf("metrics file %s is empty", path)
	}
	var record map[string]interface{}
	if err := json.Unmarshal([]byte(lines[len(lines)-1]), &record); err != nil {
		return err
	}
	for _, key := range []string{"utc_timestamp_ms", "api_server", "get_api_requests", "put_api_requests", "logger", "vmm", "hephaestus"} {
		if _, ok := record[key]; !ok {
			return fmt.Errorf("metrics %s missing key %q", path, key)
		}
	}
	fmt.Printf("    metrics=%s lines=%d bytes=%d\n", path, len(lines), len(payload))
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
