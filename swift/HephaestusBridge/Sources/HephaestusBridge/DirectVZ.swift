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

private final class ExitCodeBox: @unchecked Sendable {
    /// The guest's exit code as parsed from the serial sentinel, or -1 if
    /// the VM halted without emitting one (e.g., kernel panic before agent
    /// ran, VZ crashed).
    var code: Int32 = -1
}

// =============================================================================
// FFI: hb_vz_exec — boot direct-VZ with our guest agent initramfs, run a
// single command inside the provided rootfs, capture exit code. No vminitd,
// no containerization.
// =============================================================================

/// Port the guest agent listens on for the command channel. Kept in sync
/// with `guest/hephaestus-agent/src/main.rs::COMMAND_PORT`.
private let agentCommandPort: UInt32 = 1234

@_cdecl("hb_vz_exec")
public func hb_vz_exec(
    kernelPath: UnsafePointer<CChar>?,
    initramfsPath: UnsafePointer<CChar>?,
    rootfsPath: UnsafePointer<CChar>?,
    commandUtf8: UnsafePointer<CChar>?,
    logPath: UnsafePointer<CChar>?,
    cpuCount: UInt32,
    memoryMib: UInt64,
    timeoutSeconds: UInt32,
    outExitCode: UnsafeMutablePointer<Int32>?,
    outErr: UnsafeMutablePointer<UnsafeMutablePointer<CChar>?>?
) -> Int32 {
    guard let kernelPath, let initramfsPath, let rootfsPath, let commandUtf8 else {
        writeError(outErr, "null path or command argument")
        return Status.invalidArgument
    }
    let kernel = URL(fileURLWithPath: String(cString: kernelPath))
    let initramfs = URL(fileURLWithPath: String(cString: initramfsPath))
    let rootfs = URL(fileURLWithPath: String(cString: rootfsPath))
    let command = String(cString: commandUtf8)
    let log: URL? = logPath.map { URL(fileURLWithPath: String(cString: $0)) }

    do {
        let session = try ExecSession.make(
            kernel: kernel,
            initramfs: initramfs,
            rootfs: rootfs,
            logURL: log,
            cpuCount: Int(cpuCount == 0 ? 2 : cpuCount),
            memoryBytes: (memoryMib == 0 ? 512 : memoryMib) * (1 << 20)
        )
        let config = session.config
        // Retain the session for the rest of this function so the serial
        // pipe + log handle stay alive.
        _ = session

        let queue = DispatchQueue(label: "com.hephaestus.vz-exec")
        let holder = VMHolder()
        queue.sync {
            holder.vm = VZVirtualMachine(configuration: config, queue: queue)
        }
        guard holder.vm != nil else {
            writeError(outErr, "failed to construct VZVirtualMachine")
            return Status.swiftError
        }

        // KVO on state catches crashes / unexpected stops.
        let stopSem = DispatchSemaphore(value: 0)
        let observerBox = ObservationBox()
        queue.sync {
            guard let vm = holder.vm else { return }
            observerBox.observation = vm.observe(\.state, options: [.new]) { vm, _ in
                if vm.state == .stopped || vm.state == .error {
                    stopSem.signal()
                }
            }
        }

        try blockingStart(queue: queue, holder: holder)

        // Connect to the agent's vsock listener, send the command, read the
        // exit code. Retries a few times so the host-side connect doesn't
        // race against the agent finishing its mount+listen.
        let exitCode = try sendCommandAndAwaitExit(
            holder: holder,
            queue: queue,
            command: command
        )

        // Agent halts after responding, so wait for the clean VZ shutdown.
        let timeout = DispatchTime.now() + .seconds(Int(timeoutSeconds == 0 ? 30 : timeoutSeconds))
        _ = stopSem.wait(timeout: timeout)

        session.close()
        _ = blockingStop(queue: queue, holder: holder)
        queue.sync {
            observerBox.observation?.invalidate()
            observerBox.observation = nil
        }

        outExitCode?.pointee = exitCode
        outErr?.pointee = nil
        return Status.ok
    } catch {
        writeError(outErr, formatError(error))
        return Status.swiftError
    }
}

// =============================================================================
// ExecSession: holds the VM config and all the resources (pipes, log
// handles) that need to outlive `buildExecConfig`'s return. Stored on the
// stack of the caller so ARC keeps the pipe/file alive for the session.
// =============================================================================

// =============================================================================
// FFI: hb_vz_exec_snapshot_save — pre-warm a VM that's ready to accept
// commands, then save its state. Pair with hb_vz_exec_snapshot_restore.
// =============================================================================

@_cdecl("hb_vz_exec_snapshot_save")
public func hb_vz_exec_snapshot_save(
    kernelPath: UnsafePointer<CChar>?,
    initramfsPath: UnsafePointer<CChar>?,
    rootfsPath: UnsafePointer<CChar>?,
    savePath: UnsafePointer<CChar>?,
    logPath: UnsafePointer<CChar>?,
    cpuCount: UInt32,
    memoryMib: UInt64,
    outErr: UnsafeMutablePointer<UnsafeMutablePointer<CChar>?>?
) -> Int32 {
    guard let kernelPath, let initramfsPath, let rootfsPath, let savePath else {
        writeError(outErr, "null path argument")
        return Status.invalidArgument
    }
    let kernel = URL(fileURLWithPath: String(cString: kernelPath))
    let initramfs = URL(fileURLWithPath: String(cString: initramfsPath))
    let rootfs = URL(fileURLWithPath: String(cString: rootfsPath))
    let save = URL(fileURLWithPath: String(cString: savePath))
    let log: URL? = logPath.map { URL(fileURLWithPath: String(cString: $0)) }

    do {
        let session = try ExecSession.makeSnapshotable(
            kernel: kernel,
            initramfs: initramfs,
            rootfs: rootfs,
            logURL: log,
            machineIdURL: machineIdURL(forSavePath: save),
            cpuCount: Int(cpuCount == 0 ? 2 : cpuCount),
            memoryBytes: (memoryMib == 0 ? 512 : memoryMib) * (1 << 20)
        )
        defer { session.close() }

        let queue = DispatchQueue(label: "com.hephaestus.vz-exec-save")
        let holder = VMHolder()
        queue.sync {
            holder.vm = VZVirtualMachine(configuration: session.config, queue: queue)
        }
        guard holder.vm != nil else {
            writeError(outErr, "failed to construct VZVirtualMachine")
            return Status.swiftError
        }

        try blockingStart(queue: queue, holder: holder)

        // Probe-connect until the agent is listening. Each failed connect
        // just retries; the first success means the agent has reached
        // accept() — exactly the steady state we want to snapshot.
        let probe = try connectToAgent(holder: holder, queue: queue)
        probe.close()
        // Give the agent a brief moment to go back to accept() after
        // reading EOF from the probe; otherwise the snapshot might capture
        // mid-cleanup state which restores weirdly.
        usleep(200_000)

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
// FFI: hb_vz_exec_snapshot_restore — restore a pre-warmed VM, send it a
// command over vsock, collect the exit code.
// =============================================================================

@_cdecl("hb_vz_exec_snapshot_restore")
public func hb_vz_exec_snapshot_restore(
    kernelPath: UnsafePointer<CChar>?,
    initramfsPath: UnsafePointer<CChar>?,
    rootfsPath: UnsafePointer<CChar>?,
    savePath: UnsafePointer<CChar>?,
    commandUtf8: UnsafePointer<CChar>?,
    logPath: UnsafePointer<CChar>?,
    cpuCount: UInt32,
    memoryMib: UInt64,
    outExitCode: UnsafeMutablePointer<Int32>?,
    outRestoreNanos: UnsafeMutablePointer<UInt64>?,
    outErr: UnsafeMutablePointer<UnsafeMutablePointer<CChar>?>?
) -> Int32 {
    guard let kernelPath, let initramfsPath, let rootfsPath, let savePath, let commandUtf8 else {
        writeError(outErr, "null path or command argument")
        return Status.invalidArgument
    }
    let kernel = URL(fileURLWithPath: String(cString: kernelPath))
    let initramfs = URL(fileURLWithPath: String(cString: initramfsPath))
    let rootfs = URL(fileURLWithPath: String(cString: rootfsPath))
    let save = URL(fileURLWithPath: String(cString: savePath))
    let command = String(cString: commandUtf8)
    let log: URL? = logPath.map { URL(fileURLWithPath: String(cString: $0)) }

    do {
        let session = try ExecSession.makeSnapshotable(
            kernel: kernel,
            initramfs: initramfs,
            rootfs: rootfs,
            logURL: log,
            machineIdURL: machineIdURL(forSavePath: save),
            cpuCount: Int(cpuCount == 0 ? 2 : cpuCount),
            memoryBytes: (memoryMib == 0 ? 512 : memoryMib) * (1 << 20)
        )
        defer { session.close() }

        let queue = DispatchQueue(label: "com.hephaestus.vz-exec-restore")
        let holder = VMHolder()
        queue.sync {
            holder.vm = VZVirtualMachine(configuration: session.config, queue: queue)
        }
        guard holder.vm != nil else {
            writeError(outErr, "failed to construct VZVirtualMachine")
            return Status.swiftError
        }

        // Time restore + resume — the headline metric for the warm-start
        // story.
        let restoreStart = DispatchTime.now()
        try blockingRestore(queue: queue, holder: holder, from: save)
        try blockingResume(queue: queue, holder: holder)
        let restoreElapsed = DispatchTime.now().uptimeNanoseconds - restoreStart.uptimeNanoseconds
        outRestoreNanos?.pointee = restoreElapsed

        let exitCode = try sendCommandAndAwaitExit(
            holder: holder,
            queue: queue,
            command: command
        )

        // Give the agent a beat to halt the VM cleanly.
        usleep(100_000)
        _ = blockingStop(queue: queue, holder: holder)

        outExitCode?.pointee = exitCode
        outErr?.pointee = nil
        return Status.ok
    } catch {
        writeError(outErr, formatError(error))
        return Status.swiftError
    }
}

private final class ExecSession: @unchecked Sendable {
    let config: VZVirtualMachineConfiguration
    let outputPipe: Pipe
    let logHandle: FileHandle?

    private init(
        config: VZVirtualMachineConfiguration,
        outputPipe: Pipe,
        logHandle: FileHandle?
    ) {
        self.config = config
        self.outputPipe = outputPipe
        self.logHandle = logHandle
    }

    static func make(
        kernel: URL,
        initramfs: URL,
        rootfs: URL,
        logURL: URL?,
        cpuCount: Int,
        memoryBytes: UInt64
    ) throws -> ExecSession {
        let config = VZVirtualMachineConfiguration()
        config.cpuCount = cpuCount
        config.memorySize = memoryBytes

        let bootloader = VZLinuxBootLoader(kernelURL: kernel)
        bootloader.initialRamdiskURL = initramfs
        // `rdinit=/init` execs our agent out of the initramfs. Commands
        // now arrive over vsock after boot (see hb_vz_exec) — no more
        // baking into the kernel cmdline.
        bootloader.commandLine = "console=hvc0 rdinit=/init quiet loglevel=3"
        config.bootLoader = bootloader

        let rootfsAttachment = try VZDiskImageStorageDeviceAttachment(
            url: rootfs,
            readOnly: false
        )
        config.storageDevices = [VZVirtioBlockDeviceConfiguration(attachment: rootfsAttachment)]

        // Serial port streams guest stdio (agent diagnostics + guest
        // command stdout) to host stdout, tee'd to `logURL` if supplied.
        let outputPipe = Pipe()
        let logHandle: FileHandle? = logURL.flatMap { url in
            FileManager.default.createFile(atPath: url.path, contents: nil)
            return try? FileHandle(forWritingTo: url)
        }
        outputPipe.fileHandleForReading.readabilityHandler = { handle in
            let data = handle.availableData
            if data.isEmpty { return }
            try? logHandle?.write(contentsOf: data)
            try? FileHandle.standardOutput.write(contentsOf: data)
        }
        let serial = VZVirtioConsoleDeviceSerialPortConfiguration()
        serial.attachment = VZFileHandleSerialPortAttachment(
            fileHandleForReading: nil,
            fileHandleForWriting: outputPipe.fileHandleForWriting
        )
        config.serialPorts = [serial]

        config.socketDevices = [VZVirtioSocketDeviceConfiguration()]
        config.entropyDevices = [VZVirtioEntropyDeviceConfiguration()]
        try config.validate()

        return ExecSession(config: config, outputPipe: outputPipe, logHandle: logHandle)
    }

    func close() {
        outputPipe.fileHandleForReading.readabilityHandler = nil
        try? logHandle?.close()
    }

    /// Snapshot-compatible variant: URL-based serial attachment (the
    /// FileHandle one doesn't survive save/restore), persistent machine
    /// identifier, validated for save/restore support. Callers lose the
    /// live stdout streaming the cold-exec path gets; the log file is
    /// where output lands.
    static func makeSnapshotable(
        kernel: URL,
        initramfs: URL,
        rootfs: URL,
        logURL: URL?,
        machineIdURL: URL,
        cpuCount: Int,
        memoryBytes: UInt64
    ) throws -> ExecSession {
        let config = VZVirtualMachineConfiguration()
        config.cpuCount = cpuCount
        config.memorySize = memoryBytes

        // Persistent machine identity: save captures it, restore reuses it.
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
        bootloader.initialRamdiskURL = initramfs
        bootloader.commandLine = "console=hvc0 rdinit=/init quiet loglevel=3"
        config.bootLoader = bootloader

        let rootfsAttachment = try VZDiskImageStorageDeviceAttachment(
            url: rootfs,
            readOnly: false
        )
        config.storageDevices = [VZVirtioBlockDeviceConfiguration(attachment: rootfsAttachment)]

        // URL-based serial: VZ can serialize this across save/restore.
        // User can `tail -f` the log to see output in real time.
        let serialLog = logURL ?? URL(fileURLWithPath: NSTemporaryDirectory())
            .appendingPathComponent("hephaestus-vz-exec-\(UUID().uuidString).log")
        FileManager.default.createFile(atPath: serialLog.path, contents: nil)
        let serial = VZVirtioConsoleDeviceSerialPortConfiguration()
        serial.attachment = try VZFileSerialPortAttachment(url: serialLog, append: true)
        config.serialPorts = [serial]

        config.socketDevices = [VZVirtioSocketDeviceConfiguration()]
        config.entropyDevices = [VZVirtioEntropyDeviceConfiguration()]
        try config.validate()
        if #available(macOS 14.0, *) {
            try config.validateSaveRestoreSupport()
        }

        // No Pipe → no readabilityHandler, no logHandle to close on our side.
        return ExecSession(config: config, outputPipe: Pipe(), logHandle: nil)
    }
}

private enum VsockError: Error, CustomStringConvertible {
    case noSocketDevice
    case connectTimedOut(attempts: Int)
    case shortRead
    case connect(Error)
    case write(Error)
    var description: String {
        switch self {
        case .noSocketDevice: return "VM has no virtio-vsock device configured"
        case .connectTimedOut(let n): return "vsock connect timed out after \(n) attempts"
        case .shortRead: return "short read on vsock exit-code response"
        case .connect(let e): return "vsock connect failed: \(e)"
        case .write(let e): return "vsock write failed: \(e)"
        }
    }
}

/// Connect to the guest agent's vsock listener, send the command, read the
/// exit code i32 little-endian, close. Retries the initial connect for up
/// to ~5 seconds so the host doesn't race against the guest's `listen()`.
private func sendCommandAndAwaitExit(
    holder: VMHolder,
    queue: DispatchQueue,
    command: String
) throws -> Int32 {
    let connection = try connectToAgent(holder: holder, queue: queue)
    defer { connection.close() }

    let fd = connection.fileDescriptor
    // Command frame: u32 LE length + UTF-8 bytes.
    let body = Array(command.utf8)
    var header = UInt32(body.count).littleEndian
    try withUnsafeBytes(of: &header) { hdr in
        try writeAll(fd: fd, bytes: hdr)
    }
    try body.withUnsafeBufferPointer { buf in
        try writeAll(fd: fd, bytes: UnsafeRawBufferPointer(buf))
    }

    // Response: i32 LE exit code.
    var codeBytes = [UInt8](repeating: 0, count: 4)
    try codeBytes.withUnsafeMutableBufferPointer { buf in
        try readAll(fd: fd, into: UnsafeMutableRawBufferPointer(buf))
    }
    let code = Int32(bitPattern:
        UInt32(codeBytes[0])
        | (UInt32(codeBytes[1]) << 8)
        | (UInt32(codeBytes[2]) << 16)
        | (UInt32(codeBytes[3]) << 24)
    )
    return code
}

private func connectToAgent(
    holder: VMHolder,
    queue: DispatchQueue
) throws -> VZVirtioSocketConnection {
    // 50 × 100ms = 5s of retries. Mount + listen is usually done in <200ms.
    let maxAttempts = 50
    var lastError: Error?
    for _ in 0..<maxAttempts {
        do {
            return try connectOnce(holder: holder, queue: queue)
        } catch {
            lastError = error
            usleep(100_000)
        }
    }
    throw lastError ?? VsockError.connectTimedOut(attempts: maxAttempts)
}

private func connectOnce(
    holder: VMHolder,
    queue: DispatchQueue
) throws -> VZVirtioSocketConnection {
    let sem = DispatchSemaphore(value: 0)
    let resultBox = ConnectionResultBox()
    queue.async {
        guard let vm = holder.vm,
              let socketDevice = vm.socketDevices.first as? VZVirtioSocketDevice
        else {
            resultBox.result = .failure(VsockError.noSocketDevice)
            sem.signal()
            return
        }
        socketDevice.connect(toPort: agentCommandPort) { result in
            switch result {
            case .success(let conn):
                resultBox.result = .success(conn)
            case .failure(let err):
                resultBox.result = .failure(VsockError.connect(err))
            }
            sem.signal()
        }
    }
    sem.wait()
    switch resultBox.result {
    case .success(let conn): return conn
    case .failure(let err): throw err
    case nil: throw VsockError.noSocketDevice
    }
}

private final class ConnectionResultBox: @unchecked Sendable {
    var result: Result<VZVirtioSocketConnection, Error>?
}

private func writeAll(fd: Int32, bytes: UnsafeRawBufferPointer) throws {
    var remaining = bytes.count
    var offset = 0
    while remaining > 0 {
        let n = write(fd, bytes.baseAddress!.advanced(by: offset), remaining)
        if n < 0 {
            throw VsockError.write(POSIXError(.init(rawValue: errno) ?? .EIO))
        }
        offset += n
        remaining -= n
    }
}

private func readAll(fd: Int32, into buf: UnsafeMutableRawBufferPointer) throws {
    var remaining = buf.count
    var offset = 0
    while remaining > 0 {
        let n = read(fd, buf.baseAddress!.advanced(by: offset), remaining)
        if n < 0 {
            throw VsockError.write(POSIXError(.init(rawValue: errno) ?? .EIO))
        }
        if n == 0 {
            throw VsockError.shortRead
        }
        offset += n
        remaining -= n
    }
}

// =============================================================================
// FFI: hb_vz_long_* — long-running, client-controlled VM lifecycle.
//
// Unlike hb_vz_boot (timeout-driven) or hb_vz_exec (command-over-vsock),
// this surface exposes start/stop as separate callable steps so an HTTP
// client — hephaestus-firecracker over a Firecracker-compat socket — can
// own the VM's lifetime. The shape mirrors the containerization-backed
// `hb_vm_*` lifecycle but drives a bare VZVirtualMachine directly.
//
// Caller retains the opaque `HbVzVm *` handle produced by hb_vz_long_new
// and frees it with hb_vz_long_free. Dropping the handle without calling
// stop first is tolerated: hb_vz_long_free best-effort stops before
// releasing the Swift-side retain.
// =============================================================================

/// Swift-side holder for a long-running direct-VZ VM. `@unchecked
/// Sendable` because we serialize all VZVirtualMachine access on a
/// per-handle dispatch queue; Swift 6 strict concurrency can't prove
/// that by itself.
private final class VzVmHandle: @unchecked Sendable {
    let queue: DispatchQueue
    let holder: VMHolder
    /// Retained to keep the serial-log file URL and machine-id URL alive
    /// for the VM's lifetime. `config` itself holds the serial port
    /// attachment which references the log URL.
    let config: VZVirtualMachineConfiguration
    let logURL: URL
    let machineIdURL: URL

    init(
        queue: DispatchQueue,
        holder: VMHolder,
        config: VZVirtualMachineConfiguration,
        logURL: URL,
        machineIdURL: URL
    ) {
        self.queue = queue
        self.holder = holder
        self.config = config
        self.logURL = logURL
        self.machineIdURL = machineIdURL
    }
}

/// Build a VZVirtualMachineConfiguration for the long-running path.
/// Differs from `buildConfig` in that the caller supplies the command
/// line verbatim (no kernel cmdline defaults baked in) and an optional
/// initrd path.
private func buildLongRunningConfig(
    kernel: URL,
    rootfs: URL,
    initrd: URL?,
    logURL: URL,
    machineIdURL: URL,
    cpuCount: Int,
    memoryBytes: UInt64,
    commandLine: String
) throws -> VZVirtualMachineConfiguration {
    let config = VZVirtualMachineConfiguration()
    config.cpuCount = cpuCount
    config.memorySize = memoryBytes

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
    if let initrd {
        bootloader.initialRamdiskURL = initrd
    }
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

@_cdecl("hb_vz_long_new")
public func hb_vz_long_new(
    kernelPath: UnsafePointer<CChar>?,
    rootfsPath: UnsafePointer<CChar>?,
    initrdPath: UnsafePointer<CChar>?,
    logPath: UnsafePointer<CChar>?,
    bootArgs: UnsafePointer<CChar>?,
    cpuCount: UInt32,
    memoryMib: UInt64,
    outVm: UnsafeMutablePointer<UnsafeMutablePointer<HbVzVm>?>?,
    outErr: UnsafeMutablePointer<UnsafeMutablePointer<CChar>?>?
) -> Int32 {
    guard let kernelPath, let rootfsPath, let logPath, let bootArgs, let outVm else {
        writeError(outErr, "null path/out argument")
        return Status.invalidArgument
    }

    let kernel = URL(fileURLWithPath: String(cString: kernelPath))
    let rootfs = URL(fileURLWithPath: String(cString: rootfsPath))
    let log = URL(fileURLWithPath: String(cString: logPath))
    let initrd: URL? = initrdPath.map { URL(fileURLWithPath: String(cString: $0)) }
    let commandLine = String(cString: bootArgs)

    // Machine-id file lives next to the log. Persisted across calls so a
    // future "snapshot this long-running VM" feature doesn't trip over
    // VZ's "identifier must match at restore" invariant.
    let machineId = log.deletingPathExtension().appendingPathExtension("machineid")

    do {
        let config = try buildLongRunningConfig(
            kernel: kernel,
            rootfs: rootfs,
            initrd: initrd,
            logURL: log,
            machineIdURL: machineId,
            cpuCount: Int(cpuCount == 0 ? 2 : cpuCount),
            memoryBytes: (memoryMib == 0 ? 512 : memoryMib) * (1 << 20),
            commandLine: commandLine
        )

        let queue = DispatchQueue(label: "com.hephaestus.vz-long-\(UUID().uuidString)")
        let holder = VMHolder()
        queue.sync {
            holder.vm = VZVirtualMachine(configuration: config, queue: queue)
        }
        guard holder.vm != nil else {
            writeError(outErr, "failed to construct VZVirtualMachine")
            return Status.swiftError
        }

        let handle = VzVmHandle(
            queue: queue,
            holder: holder,
            config: config,
            logURL: log,
            machineIdURL: machineId
        )
        let opaque = Unmanaged.passRetained(handle).toOpaque()
        outVm.pointee = opaque.assumingMemoryBound(to: HbVzVm.self)
        outErr?.pointee = nil
        return Status.ok
    } catch {
        writeError(outErr, formatError(error))
        return Status.swiftError
    }
}

@_cdecl("hb_vz_long_start")
public func hb_vz_long_start(
    vm: UnsafeMutablePointer<HbVzVm>?,
    outErr: UnsafeMutablePointer<UnsafeMutablePointer<CChar>?>?
) -> Int32 {
    guard let handle = borrowVz(vm, outErr) else { return Status.invalidArgument }
    do {
        try blockingStart(queue: handle.queue, holder: handle.holder)
        outErr?.pointee = nil
        return Status.ok
    } catch {
        writeError(outErr, formatError(error))
        return Status.swiftError
    }
}

@_cdecl("hb_vz_long_stop")
public func hb_vz_long_stop(
    vm: UnsafeMutablePointer<HbVzVm>?,
    outErr: UnsafeMutablePointer<UnsafeMutablePointer<CChar>?>?
) -> Int32 {
    guard let handle = borrowVz(vm, outErr) else { return Status.invalidArgument }
    // blockingStop returns false on timeout but the VM may still be in a
    // halting state — surface that as a soft error so the caller can
    // still try to free the handle.
    let stopped = blockingStop(queue: handle.queue, holder: handle.holder)
    if stopped {
        outErr?.pointee = nil
        return Status.ok
    } else {
        writeError(outErr, "stop timed out after 5s; VM may still be halting")
        return Status.swiftError
    }
}

@_cdecl("hb_vz_long_free")
public func hb_vz_long_free(vm: UnsafeMutablePointer<HbVzVm>?) {
    guard let vm else { return }
    let opaque = UnsafeMutableRawPointer(vm)
    let handle = Unmanaged<VzVmHandle>.fromOpaque(opaque).takeUnretainedValue()
    // Best-effort stop. Ignore the result; the caller is releasing, so
    // there's nothing to report. If the VM has already stopped this is a
    // no-op; if it hasn't, this prevents a leaked dispatch queue + VZ
    // process lingering past the handle's lifetime.
    _ = blockingStop(queue: handle.queue, holder: handle.holder)
    Unmanaged<VzVmHandle>.fromOpaque(opaque).release()
}

private func borrowVz(
    _ vm: UnsafeMutablePointer<HbVzVm>?,
    _ outErr: UnsafeMutablePointer<UnsafeMutablePointer<CChar>?>?
) -> VzVmHandle? {
    guard let vm else {
        writeError(outErr, "null VzVm handle")
        return nil
    }
    let opaque = UnsafeMutableRawPointer(vm)
    return Unmanaged<VzVmHandle>.fromOpaque(opaque).takeUnretainedValue()
}
