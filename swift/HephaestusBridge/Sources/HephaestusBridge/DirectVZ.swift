// Direct Virtualization.framework path — bypasses apple/containerization
// entirely and drives VZVirtualMachine ourselves. This is the foundation
// for snapshot save/restore (N1) since containerization's VirtualMachineInstance
// protocol doesn't expose saveMachineStateTo: / restoreMachineStateFrom:.
//
// N0: boot a kernel + rootfs, stream serial console to a file, stop after a
// timeout. No vminitd, no gRPC, no process orchestration — just "did the
// kernel start?" as a smoke test.

import CHephaestusBridge
import ContainerizationOS
import Dispatch
import Foundation
import Virtualization

// =============================================================================
// VM lifecycle primitives shared across N0/N1.
// =============================================================================

private final class VMHolder: @unchecked Sendable {
    var vm: VZVirtualMachine?
    var startError: Error?
}

/// Build a minimal VZVirtualMachineConfiguration from raw paths.
///
/// `machineIdURL` is a file that persists the `VZGenericMachineIdentifier`
/// across save/restore — VZ restore refuses a different machine identifier
/// than the one the state was saved with. We create the file lazily if it
/// doesn't exist (fresh boot) and reuse whatever's there (restore path).
///
/// `logURL` receives the guest's serial console. The attachment is URL-based
/// rather than FileHandle-based so VZ can (re-)open the file on restore.
private func buildConfig(
    kernel: URL,
    rootfs: URL,
    logURL: URL,
    machineIdURL: URL,
    cpuCount: Int,
    memoryBytes: UInt64,
    commandLine: String
) throws -> VZVirtualMachineConfiguration {
    let config = VZVirtualMachineConfiguration()
    config.cpuCount = cpuCount
    config.memorySize = memoryBytes

    // Persistent machine identity for save/restore.
    let platform = VZGenericPlatformConfiguration()
    if let data = try? Data(contentsOf: machineIdURL),
       let id = VZGenericMachineIdentifier(dataRepresentation: data) {
        platform.machineIdentifier = id
    } else {
        let id = VZGenericMachineIdentifier()
        try id.dataRepresentation.write(to: machineIdURL)
        platform.machineIdentifier = id
    }
    config.platform = platform

    let bootloader = VZLinuxBootLoader(kernelURL: kernel)
    bootloader.commandLine = commandLine
    config.bootLoader = bootloader

    let rootfsAttachment = try VZDiskImageStorageDeviceAttachment(
        url: rootfs,
        readOnly: false
    )
    config.storageDevices = [VZVirtioBlockDeviceConfiguration(attachment: rootfsAttachment)]

    FileManager.default.createFile(atPath: logURL.path, contents: nil)
    let serial = VZVirtioConsoleDeviceSerialPortConfiguration()
    serial.attachment = try VZFileSerialPortAttachment(url: logURL, append: true)
    config.serialPorts = [serial]

    config.entropyDevices = [VZVirtioEntropyDeviceConfiguration()]

    try config.validate()
    if #available(macOS 14.0, *) {
        try config.validateSaveRestoreSupport()
    }
    return config
}

/// Default path for the machine identifier paired with a save file.
private func machineIdURL(forSavePath save: URL) -> URL {
    save.deletingPathExtension().appendingPathExtension("machineid")
}

// =============================================================================
// FFI: hb_vz_boot — N0 smoke entry.
// =============================================================================

@_cdecl("hb_vz_boot")
public func hb_vz_boot(
    kernelPath: UnsafePointer<CChar>?,
    rootfsPath: UnsafePointer<CChar>?,
    logPath: UnsafePointer<CChar>?,
    cpuCount: UInt32,
    memoryMib: UInt64,
    runSeconds: UInt32,
    outErr: UnsafeMutablePointer<UnsafeMutablePointer<CChar>?>?
) -> Int32 {
    guard let kernelPath, let rootfsPath, let logPath else {
        writeError(outErr, "null path argument")
        return Status.invalidArgument
    }

    let kernel = URL(fileURLWithPath: String(cString: kernelPath))
    let rootfs = URL(fileURLWithPath: String(cString: rootfsPath))
    let log = URL(fileURLWithPath: String(cString: logPath))

    do {
        // vz-boot doesn't persist machine identity across runs — fresh
        // identifier every call, written to a temp path we then ignore.
        let config = try buildConfig(
            kernel: kernel,
            rootfs: rootfs,
            logURL: log,
            machineIdURL: URL(fileURLWithPath: NSTemporaryDirectory())
                .appendingPathComponent("hephaestus-vz-boot-\(UUID().uuidString).machineid"),
            cpuCount: Int(cpuCount == 0 ? 2 : cpuCount),
            memoryBytes: (memoryMib == 0 ? 512 : memoryMib) * (1 << 20),
            // `panic=1` reboots on kernel panic instead of hanging; `console=hvc0`
            // pipes early boot output to our serial port.
            commandLine: "console=hvc0 root=/dev/vda rw init=/bin/sh panic=1"
        )

        let queue = DispatchQueue(label: "com.hephaestus.vz-boot")
        let holder = VMHolder()
        queue.sync {
            holder.vm = VZVirtualMachine(configuration: config, queue: queue)
        }
        guard holder.vm != nil else {
            writeError(outErr, "failed to construct VZVirtualMachine")
            return Status.swiftError
        }

        // Start the VM and block until the completion handler fires. We
        // never capture `vm` across a @Sendable boundary — VZVirtualMachine
        // isn't Sendable — so the closures read it from the holder, which
        // is our @unchecked Sendable escape hatch.
        let startSem = DispatchSemaphore(value: 0)
        queue.async {
            holder.vm?.start { result in
                if case .failure(let err) = result {
                    holder.startError = err
                }
                startSem.signal()
            }
        }
        startSem.wait()
        if let err = holder.startError {
            throw err
        }

        // Give the kernel time to print its boot log (init=/bin/sh will
        // likely fail on a container rootfs that lacks a shell at that
        // path; panic=1 makes it reboot so the log keeps growing).
        Thread.sleep(forTimeInterval: Double(runSeconds == 0 ? 5 : runSeconds))

        // Request graceful stop, then force-stop. The container rootfs has
        // no ACPI-aware init, so the graceful request is mostly theatre.
        let stopSem = DispatchSemaphore(value: 0)
        queue.async {
            guard let vm = holder.vm else {
                stopSem.signal()
                return
            }
            if vm.canRequestStop {
                _ = try? vm.requestStop()
            }
            vm.stop { _ in
                stopSem.signal()
            }
        }
        _ = stopSem.wait(timeout: .now() + 5)

        outErr?.pointee = nil
        return Status.ok
    } catch {
        writeError(outErr, formatError(error))
        return Status.swiftError
    }
}

// =============================================================================
// FFI: hb_vz_snapshot_save — boot a VM, let it settle, pause + save state.
// =============================================================================

@_cdecl("hb_vz_snapshot_save")
public func hb_vz_snapshot_save(
    kernelPath: UnsafePointer<CChar>?,
    rootfsPath: UnsafePointer<CChar>?,
    logPath: UnsafePointer<CChar>?,
    savePath: UnsafePointer<CChar>?,
    cpuCount: UInt32,
    memoryMib: UInt64,
    settleSeconds: UInt32,
    outErr: UnsafeMutablePointer<UnsafeMutablePointer<CChar>?>?
) -> Int32 {
    guard let kernelPath, let rootfsPath, let logPath, let savePath else {
        writeError(outErr, "null path argument")
        return Status.invalidArgument
    }
    let kernel = URL(fileURLWithPath: String(cString: kernelPath))
    let rootfs = URL(fileURLWithPath: String(cString: rootfsPath))
    let log = URL(fileURLWithPath: String(cString: logPath))
    let save = URL(fileURLWithPath: String(cString: savePath))

    do {
        let config = try buildConfig(
            kernel: kernel,
            rootfs: rootfs,
            logURL: log,
            machineIdURL: machineIdURL(forSavePath: save),
            cpuCount: Int(cpuCount == 0 ? 2 : cpuCount),
            memoryBytes: (memoryMib == 0 ? 512 : memoryMib) * (1 << 20),
            commandLine: "console=hvc0 root=/dev/vda rw init=/bin/sh panic=1"
        )

        let queue = DispatchQueue(label: "com.hephaestus.vz-snapshot-save")
        let holder = VMHolder()
        queue.sync {
            holder.vm = VZVirtualMachine(configuration: config, queue: queue)
        }
        guard holder.vm != nil else {
            writeError(outErr, "failed to construct VZVirtualMachine")
            return Status.swiftError
        }

        // Start.
        try blockingStart(queue: queue, holder: holder)

        // Let the guest reach a steady-ish state before we snapshot.
        Thread.sleep(forTimeInterval: Double(settleSeconds == 0 ? 3 : settleSeconds))

        // Pause → save → stop. saveMachineStateTo: requires pause.
        try blockingPause(queue: queue, holder: holder)
        try blockingSave(queue: queue, holder: holder, to: save)
        _ = blockingStop(queue: queue, holder: holder)

        outErr?.pointee = nil
        return Status.ok
    } catch {
        writeError(outErr, formatError(error))
        return Status.swiftError
    }
}

// =============================================================================
// FFI: hb_vz_snapshot_restore — build an identical config, restore from
// file, resume, run for `runSeconds`, stop.
// =============================================================================

@_cdecl("hb_vz_snapshot_restore")
public func hb_vz_snapshot_restore(
    kernelPath: UnsafePointer<CChar>?,
    rootfsPath: UnsafePointer<CChar>?,
    logPath: UnsafePointer<CChar>?,
    savePath: UnsafePointer<CChar>?,
    cpuCount: UInt32,
    memoryMib: UInt64,
    runSeconds: UInt32,
    outRestoreNanos: UnsafeMutablePointer<UInt64>?,
    outErr: UnsafeMutablePointer<UnsafeMutablePointer<CChar>?>?
) -> Int32 {
    guard let kernelPath, let rootfsPath, let logPath, let savePath else {
        writeError(outErr, "null path argument")
        return Status.invalidArgument
    }
    let kernel = URL(fileURLWithPath: String(cString: kernelPath))
    let rootfs = URL(fileURLWithPath: String(cString: rootfsPath))
    let log = URL(fileURLWithPath: String(cString: logPath))
    let save = URL(fileURLWithPath: String(cString: savePath))

    do {
        // Config MUST match whatever was saved (same kernel, same devices,
        // same memory, same machine identifier). We use the same builder
        // and the sibling .machineid file so this is automatic as long as
        // args match the save call.
        let config = try buildConfig(
            kernel: kernel,
            rootfs: rootfs,
            logURL: log,
            machineIdURL: machineIdURL(forSavePath: save),
            cpuCount: Int(cpuCount == 0 ? 2 : cpuCount),
            memoryBytes: (memoryMib == 0 ? 512 : memoryMib) * (1 << 20),
            commandLine: "console=hvc0 root=/dev/vda rw init=/bin/sh panic=1"
        )

        let queue = DispatchQueue(label: "com.hephaestus.vz-snapshot-restore")
        let holder = VMHolder()
        queue.sync {
            holder.vm = VZVirtualMachine(configuration: config, queue: queue)
        }
        guard holder.vm != nil else {
            writeError(outErr, "failed to construct VZVirtualMachine")
            return Status.swiftError
        }

        // Time the actual restore call — this is the number we care about
        // for the "fast boot via snapshot" story.
        let restoreStart = DispatchTime.now()
        try blockingRestore(queue: queue, holder: holder, from: save)
        try blockingResume(queue: queue, holder: holder)
        let restoreElapsed = DispatchTime.now().uptimeNanoseconds - restoreStart.uptimeNanoseconds
        outRestoreNanos?.pointee = restoreElapsed

        Thread.sleep(forTimeInterval: Double(runSeconds == 0 ? 3 : runSeconds))
        _ = blockingStop(queue: queue, holder: holder)

        outErr?.pointee = nil
        return Status.ok
    } catch {
        writeError(outErr, formatError(error))
        return Status.swiftError
    }
}

// =============================================================================
// Async → sync adapters for VZVirtualMachine lifecycle. Each schedules on
// the VM's dispatch queue and blocks the caller on a semaphore. The holder
// carries the non-Sendable VZVirtualMachine across the Sendable boundary.
// =============================================================================

private enum BlockingError: Error, CustomStringConvertible {
    case noVM
    case cancelled
    var description: String {
        switch self {
        case .noVM: return "VZVirtualMachine holder is empty"
        case .cancelled: return "operation cancelled"
        }
    }
}

/// Reference-typed box for carrying a mutable `Error?` into a @Sendable
/// closure without tripping Swift 6's concurrency checks. The block-and-wait
/// helpers below never run their closures concurrently with the outer read,
/// so @unchecked Sendable is sound.
private final class ErrorBox: @unchecked Sendable {
    var value: Error?
}

private func blockingStart(queue: DispatchQueue, holder: VMHolder) throws {
    let sem = DispatchSemaphore(value: 0)
    queue.async {
        guard let vm = holder.vm else { sem.signal(); return }
        vm.start { result in
            if case .failure(let err) = result { holder.startError = err }
            sem.signal()
        }
    }
    sem.wait()
    if let err = holder.startError {
        holder.startError = nil
        throw err
    }
}

private func blockingPause(queue: DispatchQueue, holder: VMHolder) throws {
    let sem = DispatchSemaphore(value: 0)
    let box = ErrorBox()
    queue.async {
        guard let vm = holder.vm else { sem.signal(); return }
        vm.pause { result in
            if case .failure(let err) = result { box.value = err }
            sem.signal()
        }
    }
    sem.wait()
    if let err = box.value { throw err }
}

private func blockingResume(queue: DispatchQueue, holder: VMHolder) throws {
    let sem = DispatchSemaphore(value: 0)
    let box = ErrorBox()
    queue.async {
        guard let vm = holder.vm else { sem.signal(); return }
        vm.resume { result in
            if case .failure(let err) = result { box.value = err }
            sem.signal()
        }
    }
    sem.wait()
    if let err = box.value { throw err }
}

private func blockingSave(queue: DispatchQueue, holder: VMHolder, to url: URL) throws {
    let sem = DispatchSemaphore(value: 0)
    let box = ErrorBox()
    queue.async {
        guard let vm = holder.vm else { sem.signal(); return }
        vm.saveMachineStateTo(url: url) { err in
            if let err { box.value = err }
            sem.signal()
        }
    }
    sem.wait()
    if let err = box.value { throw err }
}

private func blockingRestore(queue: DispatchQueue, holder: VMHolder, from url: URL) throws {
    let sem = DispatchSemaphore(value: 0)
    let box = ErrorBox()
    queue.async {
        guard let vm = holder.vm else { sem.signal(); return }
        vm.restoreMachineStateFrom(url: url) { err in
            if let err { box.value = err }
            sem.signal()
        }
    }
    sem.wait()
    if let err = box.value { throw err }
}

private func blockingStop(queue: DispatchQueue, holder: VMHolder) -> Bool {
    let sem = DispatchSemaphore(value: 0)
    queue.async {
        guard let vm = holder.vm else { sem.signal(); return }
        if vm.canRequestStop { _ = try? vm.requestStop() }
        vm.stop { _ in sem.signal() }
    }
    return sem.wait(timeout: .now() + 5) == .success
}

// =============================================================================
// FFI: hb_vz_sh — boot direct-VZ with the host's stdin/stdout wired straight
// to the guest serial port. Gives an interactive shell without vminitd or
// any containerization-layer orchestration. Incompatible with save/restore
// (FileHandle-based serial attachments don't serialize).
// =============================================================================

@_cdecl("hb_vz_sh")
public func hb_vz_sh(
    kernelPath: UnsafePointer<CChar>?,
    rootfsPath: UnsafePointer<CChar>?,
    cpuCount: UInt32,
    memoryMib: UInt64,
    timeoutSeconds: UInt32,
    outErr: UnsafeMutablePointer<UnsafeMutablePointer<CChar>?>?
) -> Int32 {
    guard let kernelPath, let rootfsPath else {
        writeError(outErr, "null path argument")
        return Status.invalidArgument
    }
    let kernel = URL(fileURLWithPath: String(cString: kernelPath))
    let rootfs = URL(fileURLWithPath: String(cString: rootfsPath))

    do {
        let config = VZVirtualMachineConfiguration()
        config.cpuCount = Int(cpuCount == 0 ? 2 : cpuCount)
        config.memorySize = (memoryMib == 0 ? 512 : memoryMib) * (1 << 20)

        let bootloader = VZLinuxBootLoader(kernelURL: kernel)
        // `quiet loglevel=3` hides the virtio/IPVS/etc. boot firehose; only
        // warnings and the eventual "Kernel panic" sentinel (which we use to
        // detect shell exit below) remain. `panic=0` makes the kernel halt
        // instead of reboot when PID 1 exits.
        bootloader.commandLine =
            "console=hvc0 root=/dev/vda rw init=/bin/sh panic=0 quiet loglevel=3"
        config.bootLoader = bootloader

        let rootfsAttachment = try VZDiskImageStorageDeviceAttachment(
            url: rootfs,
            readOnly: false
        )
        config.storageDevices = [VZVirtioBlockDeviceConfiguration(attachment: rootfsAttachment)]

        // Intercept guest output through a Pipe so we can (a) forward it to
        // the user's stdout and (b) watch for the "Kernel panic" message
        // that follows PID 1 exiting (`exit` / Ctrl-D in the shell). Without
        // this sniffer the VZ state property never transitions to .stopped
        // on panic=0 and the CLI hangs forever.
        let outputPipe = Pipe()
        let exitSem = DispatchSemaphore(value: 0)
        let exitBox = ExitFlagBox()
        outputPipe.fileHandleForReading.readabilityHandler = { handle in
            let data = handle.availableData
            if data.isEmpty || exitBox.signalled { return }
            let isPanic = (String(data: data, encoding: .utf8) ?? "").contains("Kernel panic")
            if !isPanic {
                try? FileHandle.standardOutput.write(contentsOf: data)
            } else {
                // The panic message follows PID 1 exiting; user has seen
                // their shell end, and the kernel noise after isn't
                // interesting output. Swallow it and signal shutdown.
                exitBox.signalled = true
                exitSem.signal()
            }
        }

        let serial = VZVirtioConsoleDeviceSerialPortConfiguration()
        serial.attachment = VZFileHandleSerialPortAttachment(
            fileHandleForReading: FileHandle.standardInput,
            fileHandleForWriting: outputPipe.fileHandleForWriting
        )
        config.serialPorts = [serial]

        config.entropyDevices = [VZVirtioEntropyDeviceConfiguration()]
        try config.validate()

        // Put the host terminal in raw mode so keystrokes, including
        // Ctrl-C and Ctrl-D, land in the guest rather than being
        // interpreted by the host shell.
        let terminal = try Terminal.current
        try terminal.setraw()
        // Ensure we always reset, even on Swift error paths below.
        defer { terminal.tryReset() }

        let queue = DispatchQueue(label: "com.hephaestus.vz-sh")
        let holder = VMHolder()
        queue.sync {
            holder.vm = VZVirtualMachine(configuration: config, queue: queue)
        }
        guard holder.vm != nil else {
            writeError(outErr, "failed to construct VZVirtualMachine")
            return Status.swiftError
        }

        // KVO on state catches VM crashes and explicit stops. Panic on
        // panic=0 doesn't trigger a VZ state transition (the kernel is
        // halted but VZ considers the VM still running), so we also
        // rely on the serial-output sniffer above.
        let observerBox = ObservationBox()
        queue.sync {
            guard let vm = holder.vm else { return }
            observerBox.observation = vm.observe(\.state, options: [.new]) { vm, _ in
                if vm.state == .stopped || vm.state == .error {
                    if !exitBox.signalled {
                        exitBox.signalled = true
                        exitSem.signal()
                    }
                }
            }
        }

        try blockingStart(queue: queue, holder: holder)

        // Wait for kernel-panic-detected or user-supplied timeout.
        let timeout = DispatchTime.now() + .seconds(Int(timeoutSeconds == 0 ? 3600 : timeoutSeconds))
        _ = exitSem.wait(timeout: timeout)

        // Drop the stdio sniffer before stop so a late write can't race
        // against teardown and double-signal.
        outputPipe.fileHandleForReading.readabilityHandler = nil
        _ = blockingStop(queue: queue, holder: holder)
        queue.sync {
            observerBox.observation?.invalidate()
            observerBox.observation = nil
        }

        outErr?.pointee = nil
        return Status.ok
    } catch {
        writeError(outErr, formatError(error))
        return Status.swiftError
    }
}

private final class ObservationBox: @unchecked Sendable {
    var observation: NSKeyValueObservation?
}

private final class ExitFlagBox: @unchecked Sendable {
    var signalled = false
}
