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

private final class StdinPumpErrorBox: @unchecked Sendable {
    var error: Error?
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
    forwardStdin: UInt8,
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

        // Tear the VM + serial pipes + observer down on every exit path.
        // Previously this ran only on success, so a thrown error (agent
        // connect timeout, short read) leaked the running VM and left the
        // pipe readabilityHandlers firing guest output after we returned.
        defer {
            session.close()
            _ = blockingStop(queue: queue, holder: holder)
            queue.sync {
                observerBox.observation?.invalidate()
                observerBox.observation = nil
            }
        }

        try blockingStart(queue: queue, holder: holder)

        // Connect to the agent's vsock listener, send the command, read the
        // exit code. Retries a few times so the host-side connect doesn't
        // race against the agent finishing its mount+listen.
        let exitCode = try sendCommandAndAwaitExit(
            holder: holder,
            queue: queue,
            command: command,
            forwardStdin: forwardStdin != 0,
            stdinFd: FileHandle.standardInput.fileDescriptor,
            exitReadTimeoutSeconds: timeoutSeconds == 0 ? 30 : timeoutSeconds
        )

        // Agent halts after responding, so wait for the clean VZ shutdown.
        let timeout = DispatchTime.now() + .seconds(Int(timeoutSeconds == 0 ? 30 : timeoutSeconds))
        _ = stopSem.wait(timeout: timeout)

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

        // The snapshot-restore FFI has no timeout argument; apply a generous
        // backstop so a hung guest command still can't block the call forever.
        let exitCode = try sendCommandAndAwaitExit(
            holder: holder,
            queue: queue,
            command: command,
            forwardStdin: false,
            stdinFd: -1,
            exitReadTimeoutSeconds: 300
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
    let stderrPipe: Pipe
    let logHandle: FileHandle?

    private init(
        config: VZVirtualMachineConfiguration,
        outputPipe: Pipe,
        stderrPipe: Pipe,
        logHandle: FileHandle?
    ) {
        self.config = config
        self.outputPipe = outputPipe
        self.stderrPipe = stderrPipe
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

        // First serial port (hvc0): agent diagnostics + guest command
        // stdout, streamed to host stdout, tee'd to `logURL` if supplied.
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
        let stdoutSerial = VZVirtioConsoleDeviceSerialPortConfiguration()
        stdoutSerial.attachment = VZFileHandleSerialPortAttachment(
            fileHandleForReading: nil,
            fileHandleForWriting: outputPipe.fileHandleForWriting
        )

        // Second serial port (hvc1): guest command stderr, streamed to
        // host stderr (no log tee — the log file is for stdout only, so
        // stderr stays separable for downstream consumers). The agent
        // dups /dev/hvc1 onto fd 2 before exec'ing /bin/sh -c CMD.
        let stderrPipe = Pipe()
        stderrPipe.fileHandleForReading.readabilityHandler = { handle in
            let data = handle.availableData
            if data.isEmpty { return }
            try? FileHandle.standardError.write(contentsOf: data)
        }
        let stderrSerial = VZVirtioConsoleDeviceSerialPortConfiguration()
        stderrSerial.attachment = VZFileHandleSerialPortAttachment(
            fileHandleForReading: nil,
            fileHandleForWriting: stderrPipe.fileHandleForWriting
        )
        config.serialPorts = [stdoutSerial, stderrSerial]

        config.socketDevices = [VZVirtioSocketDeviceConfiguration()]
        config.entropyDevices = [VZVirtioEntropyDeviceConfiguration()]
        try config.validate()

        return ExecSession(
            config: config,
            outputPipe: outputPipe,
            stderrPipe: stderrPipe,
            logHandle: logHandle
        )
    }

    func close() {
        outputPipe.fileHandleForReading.readabilityHandler = nil
        stderrPipe.fileHandleForReading.readabilityHandler = nil
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

        // First serial port (hvc0): URL-based so VZ can serialize it across
        // save/restore. User can `tail -f` the log to see stdout in real time.
        let serialLog = logURL ?? URL(fileURLWithPath: NSTemporaryDirectory())
            .appendingPathComponent("hephaestus-vz-exec-\(UUID().uuidString).log")
        FileManager.default.createFile(atPath: serialLog.path, contents: nil)
        let serial = VZVirtioConsoleDeviceSerialPortConfiguration()
        serial.attachment = try VZFileSerialPortAttachment(url: serialLog, append: true)

        // Second serial port (hvc1): guest command stderr. URL-based too, so
        // it survives save/restore exactly like hvc0 — a FileHandle pipe (what
        // the cold-exec path uses to live-stream to host fd 2) would not. The
        // agent dups /dev/hvc1 onto fd 2 before exec; without this port the
        // restored guest has no hvc1 and stderr stays merged on hvc0. Stderr
        // lands in a sibling `<log>.stderr` file rather than the host's fd 2:
        // the restore path can't live-stream the way the cold path does, so
        // file-level separation is the ceiling here. The save and restore
        // configs must match shape, so both carry these two ports — which
        // means snapshots taken before this change can no longer be restored.
        let stderrLog = serialLog.appendingPathExtension("stderr")
        FileManager.default.createFile(atPath: stderrLog.path, contents: nil)
        let stderrSerial = VZVirtioConsoleDeviceSerialPortConfiguration()
        stderrSerial.attachment = try VZFileSerialPortAttachment(url: stderrLog, append: true)

        config.serialPorts = [serial, stderrSerial]

        config.socketDevices = [VZVirtioSocketDeviceConfiguration()]
        config.entropyDevices = [VZVirtioEntropyDeviceConfiguration()]
        try config.validate()
        if #available(macOS 14.0, *) {
            try config.validateSaveRestoreSupport()
        }

        // No Pipe → no readabilityHandler, no logHandle to close on our side.
        // Both serial ports are URL-based, so the host reads the split streams
        // from the `<log>` / `<log>.stderr` files rather than live pipes; the
        // inert Pipes below just satisfy the initializer.
        return ExecSession(
            config: config,
            outputPipe: Pipe(),
            stderrPipe: Pipe(),
            logHandle: nil
        )
    }
}

private enum VsockError: Error, CustomStringConvertible {
    case noSocketDevice
    case connectTimedOut(attempts: Int)
    case shortRead
    case connect(Error)
    case write(Error)
    case read(Error)
    case readTimedOut(seconds: UInt32)
    var description: String {
        switch self {
        case .noSocketDevice: return "VM has no virtio-vsock device configured"
        case .connectTimedOut(let n): return "vsock connect timed out after \(n) attempts"
        case .shortRead: return "short read on vsock exit-code response"
        case .connect(let e): return "vsock connect failed: \(e)"
        case .write(let e): return "vsock write failed: \(e)"
        case .read(let e): return "vsock read failed: \(e)"
        case .readTimedOut(let s):
            return "timed out after \(s)s waiting for the guest command's exit code over vsock"
        }
    }
}

/// Connect to the guest agent's vsock listener, send the command, read the
/// exit code i32 little-endian, close. Retries the initial connect for up
/// to ~5 seconds so the host doesn't race against the guest's `listen()`.
///
/// When `forwardStdin` is true, the host pumps bytes from `stdinFd` to the
/// guest vsock connection after sending the command frame, so the guest
/// agent can pipe them into the child's stdin. The guest signals end-of-
/// stdin by closing its read side (host closes the connection after
/// `stdinFd` returns EOF).
private func sendCommandAndAwaitExit(
    holder: VMHolder,
    queue: DispatchQueue,
    command: String,
    forwardStdin: Bool,
    stdinFd: Int32,
    exitReadTimeoutSeconds: UInt32
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

    // Stdin forwarding: pump host stdin → vsock until EOF. A dedicated
    // thread owns the reads so the exit-code read below can race against
    // it without the pump stealing the 4-byte exit-code tail. The guest
    // agent closes its end when the child exits, which causes our read
    // below to return 0 (EOF) — we read the exit code first, then the
    // pump thread observes EOF on stdin and exits.
    let pumpErrorBox = StdinPumpErrorBox()
    let pumpDone = DispatchSemaphore(value: 0)
    if forwardStdin {
        DispatchQueue.global(qos: .utility).async {
            do {
                try pumpStdinToVsock(srcFd: stdinFd, dstFd: fd)
                // Signal EOF to the guest's stdin without closing the read side
                // we still need for the 4-byte exit-code response.
                _ = shutdown(fd, SHUT_WR)
            } catch {
                pumpErrorBox.error = error
                _ = shutdown(fd, SHUT_WR)
            }
            pumpDone.signal()
        }
    }

    // Response: i32 LE exit code. Bound the read so a guest command that
    // hangs (and so never writes its exit code, and never EOFs the way a
    // crash would) can't park this call forever — that would defeat the
    // caller's `timeout_seconds`.
    setRecvTimeout(fd: fd, seconds: exitReadTimeoutSeconds)
    var codeBytes = [UInt8](repeating: 0, count: 4)
    do {
        try codeBytes.withUnsafeMutableBufferPointer { buf in
            try readAll(fd: fd, into: UnsafeMutableRawBufferPointer(buf))
        }
    } catch VsockError.readTimedOut {
        throw VsockError.readTimedOut(seconds: exitReadTimeoutSeconds)
    }
    let code = Int32(bitPattern:
        UInt32(codeBytes[0])
        | (UInt32(codeBytes[1]) << 8)
        | (UInt32(codeBytes[2]) << 16)
        | (UInt32(codeBytes[3]) << 24)
    )
    // Drain the pump thread so it doesn't race teardown.
    if forwardStdin {
        _ = pumpDone.wait(timeout: .now() + .seconds(5))
        if let err = pumpErrorBox.error { throw err }
    }
    return code
}

/// Pump bytes from `srcFd` (a POSIX file descriptor) to `dstFd` until
/// `read` returns 0 (EOF) or fails. The caller owns both fds; this
/// helper does not close or half-close them.
private func pumpStdinToVsock(srcFd: Int32, dstFd: Int32) throws {
    var buf = [UInt8](repeating: 0, count: 4096)
    while true {
        let n = buf.withUnsafeMutableBufferPointer { ptr -> ssize_t in
            read(srcFd, ptr.baseAddress, ptr.count)
        }
        if n < 0 {
            let err = errno
            if err == EINTR { continue }
            throw POSIXError(.init(rawValue: err) ?? .EIO)
        }
        if n == 0 { return }
        try buf.withUnsafeBufferPointer { ptr in
            let raw = UnsafeRawBufferPointer(start: ptr.baseAddress, count: Int(n))
            try writeAll(fd: dstFd, bytes: raw)
        }
    }
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

private enum MmdsOutputFormat {
    case json
    case imds
}

private struct MmdsHttpRequest {
    let method: String
    let path: String
    let outputFormat: MmdsOutputFormat
}

private struct MmdsHttpResponse {
    let status: String
    let contentType: String
    let body: Data
}

private final class MmdsSocketService: NSObject, VZVirtioSocketListenerDelegate, @unchecked Sendable {
    let listener = VZVirtioSocketListener()
    let body: Data
    private var connections: [VZVirtioSocketConnection] = []
    private let lock = NSLock()

    init(json: Data) {
        self.body = json
        super.init()
        listener.delegate = self
    }

    func listener(
        _ listener: VZVirtioSocketListener,
        shouldAcceptNewConnection connection: VZVirtioSocketConnection,
        from socketDevice: VZVirtioSocketDevice
    ) -> Bool {
        lock.lock()
        connections.append(connection)
        lock.unlock()
        let fd = connection.fileDescriptor
        let fallbackBody = body
        DispatchQueue.global(qos: .utility).async { [weak self, weak connection] in
            // Read to end-of-headers, not a single one-shot read: virtio-vsock
            // gives no single-segment delivery guarantee, so the guest's
            // request can arrive split across reads. A one-shot read that
            // catches only the request line silently drops the Accept header —
            // the difference between Firecracker's JSON and IMDS output
            // formats. The receive timeout bounds a guest that stalls
            // mid-request so this pool thread can't be pinned forever.
            setRecvTimeout(fd: fd, seconds: 2)
            let endOfHeaders = Data("\r\n\r\n".utf8)
            var requestBytes = Data()
            var scratch = [UInt8](repeating: 0, count: 4096)
            while requestBytes.count < 8192, requestBytes.range(of: endOfHeaders) == nil {
                let n = scratch.withUnsafeMutableBufferPointer { buf in
                    read(fd, buf.baseAddress, buf.count)
                }
                if n < 0 && errno == EINTR { continue }
                // EOF, error, or recv-timeout: parse whatever we have.
                if n <= 0 { break }
                requestBytes.append(contentsOf: scratch.prefix(n))
            }
            let response = MmdsSocketService.response(
                fallbackBody: fallbackBody,
                requestBytes: requestBytes
            )
            let header = "HTTP/1.1 \(response.status)\r\nContent-Type: \(response.contentType)\r\nContent-Length: \(response.body.count)\r\nConnection: close\r\n\r\n"
            var out = Data(header.utf8)
            out.append(response.body)
            // Full-write loop: a single write(2) may write short, which would
            // truncate the JSON body the guest sees.
            out.withUnsafeBytes { (raw: UnsafeRawBufferPointer) in
                guard let base = raw.baseAddress else { return }
                var offset = 0
                while offset < raw.count {
                    let n = write(fd, base.advanced(by: offset), raw.count - offset)
                    if n < 0 && errno == EINTR { continue }
                    if n <= 0 { break }
                    offset += n
                }
            }
            connection?.close()
            self?.lock.lock()
            if let c = connection {
                self?.connections.removeAll { $0 === c }
            }
            self?.lock.unlock()
        }
        return true
    }

    private static func response(
        fallbackBody: Data,
        requestBytes: Data
    ) -> MmdsHttpResponse {
        guard let request = parseRequest(requestBytes) else {
            return MmdsHttpResponse(status: "200 OK", contentType: "application/json", body: fallbackBody)
        }
        guard request.method == "GET" else {
            return textResponse("405 Method Not Allowed", "Method not allowed")
        }
        guard let root = try? JSONSerialization.jsonObject(with: fallbackBody, options: [.fragmentsAllowed]) else {
            return MmdsHttpResponse(status: "200 OK", contentType: "application/json", body: fallbackBody)
        }
        guard let value = lookup(root: root, path: request.path) else {
            return textResponse("404 Not Found", "The MMDS resource does not exist: \(request.path)")
        }
        switch request.outputFormat {
        case .json:
            do {
                let data = try JSONSerialization.data(withJSONObject: value, options: [.fragmentsAllowed])
                return MmdsHttpResponse(status: "200 OK", contentType: "application/json", body: data)
            } catch {
                return textResponse("500 Internal Server Error", "MMDS JSON serialization failed")
            }
        case .imds:
            guard let text = formatImds(value) else {
                return textResponse("501 Not Implemented", "Cannot retrieve value. The value has an unsupported type.")
            }
            return textResponse("200 OK", text)
        }
    }

    private static func parseRequest(_ data: Data) -> MmdsHttpRequest? {
        guard let text = String(data: data, encoding: .utf8) else { return nil }
        // Split with components(separatedBy:), NOT split(separator: "\n"):
        // Swift treats "\r\n" as a single grapheme-cluster Character, so a
        // Character-based split on "\n" never matches inside CRLF-delimited
        // HTTP headers — the request parses as one giant line and every
        // header (notably Accept) is silently dropped.
        let lines = text.components(separatedBy: "\n").map {
            $0.trimmingCharacters(in: CharacterSet(charactersIn: "\r"))
        }
        guard let requestLine = lines.first else { return nil }
        let parts = requestLine.split(separator: " ", omittingEmptySubsequences: true)
        guard parts.count >= 2 else { return nil }
        let method = String(parts[0]).uppercased()
        let target = String(parts[1])
        let path = sanitizePath(target)
        let acceptsJson = lines.dropFirst().contains { line in
            let lower = line.lowercased()
            return lower.hasPrefix("accept:") && lower.contains("application/json")
        }
        return MmdsHttpRequest(method: method, path: path, outputFormat: acceptsJson ? .json : .imds)
    }

    private static func sanitizePath(_ target: String) -> String {
        let rawPath: String
        if let url = URL(string: target), let scheme = url.scheme, !scheme.isEmpty {
            rawPath = url.path.isEmpty ? "/" : url.path
        } else {
            rawPath = String(target.split(separator: "?", maxSplits: 1, omittingEmptySubsequences: false).first ?? "/")
        }
        var path = rawPath.isEmpty ? "/" : rawPath
        while path.contains("//") {
            path = path.replacingOccurrences(of: "//", with: "/")
        }
        return path
    }

    private static func lookup(root: Any, path: String) -> Any? {
        if path == "/" || path.isEmpty {
            return root
        }
        var value: Any? = root
        let trimmed = path.hasSuffix("/") ? String(path.dropLast()) : path
        for component in trimmed.split(separator: "/").map(String.init) {
            let key = component.replacingOccurrences(of: "~1", with: "/")
                .replacingOccurrences(of: "~0", with: "~")
            if let dict = value as? [String: Any] {
                value = dict[key]
            } else if let array = value as? [Any], let idx = Int(key), array.indices.contains(idx) {
                value = array[idx]
            } else {
                return nil
            }
        }
        return value
    }

    private static func formatImds(_ value: Any) -> String? {
        if let dict = value as? [String: Any] {
            return dict.keys.sorted().map { key in
                if dict[key] is [String: Any] {
                    return "\(key)/"
                }
                return key
            }.joined(separator: "\n")
        }
        return value as? String
    }

    private static func textResponse(_ status: String, _ text: String) -> MmdsHttpResponse {
        MmdsHttpResponse(
            status: status,
            contentType: "text/plain",
            body: text.data(using: .utf8) ?? Data()
        )
    }
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
            let err = errno
            if err == EINTR { continue }
            // With SO_RCVTIMEO set, a lapsed deadline surfaces as EAGAIN/
            // EWOULDBLOCK — report it as a timeout so the caller's
            // `timeout_seconds` is honored instead of blocking forever.
            if err == EAGAIN || err == EWOULDBLOCK {
                throw VsockError.readTimedOut(seconds: 0)
            }
            throw VsockError.read(POSIXError(.init(rawValue: err) ?? .EIO))
        }
        if n == 0 {
            throw VsockError.shortRead
        }
        offset += n
        remaining -= n
    }
}

/// Best-effort receive timeout on a socket fd via `SO_RCVTIMEO`, so a blocked
/// `read` (e.g. a guest command that hangs without ever writing its exit
/// code) can't park the caller indefinitely. `seconds == 0` clears the
/// timeout (blocking). Failures are ignored: the timeout is a backstop, not a
/// correctness dependency.
private func setRecvTimeout(fd: Int32, seconds: UInt32) {
    var tv = timeval(tv_sec: Int(seconds), tv_usec: 0)
    _ = withUnsafePointer(to: &tv) { ptr in
        setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, ptr, socklen_t(MemoryLayout<timeval>.size))
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
    var socketListeners: [AnyObject] = []

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
    commandLine: String,
    readOnly: Bool,
    enableNetworking: Bool,
    macAddress: String?
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
        readOnly: readOnly
    )
    config.storageDevices = [VZVirtioBlockDeviceConfiguration(attachment: rootfsAttachment)]

    FileManager.default.createFile(atPath: logURL.path, contents: nil)
    let serial = VZVirtioConsoleDeviceSerialPortConfiguration()
    serial.attachment = try VZFileSerialPortAttachment(url: logURL, append: true)
    config.serialPorts = [serial]

    config.entropyDevices = [VZVirtioEntropyDeviceConfiguration()]
    config.socketDevices = [VZVirtioSocketDeviceConfiguration()]

    // Guest networking via VZ's built-in NAT. NAT only needs the base
    // com.apple.security.virtualization entitlement (unlike vmnet's
    // restricted com.apple.vm.networking), so it works under ad-hoc
    // signing. VZ hands the guest a DHCP lease in 192.168.64.0/24.
    if enableNetworking {
        let netDevice = VZVirtioNetworkDeviceConfiguration()
        netDevice.attachment = VZNATNetworkDeviceAttachment()
        if let macAddress, let mac = VZMACAddress(string: macAddress) {
            netDevice.macAddress = mac
        }
        config.networkDevices = [netDevice]
    }

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
    readOnly: Bool,
    enableNetworking: Bool,
    macAddress: UnsafePointer<CChar>?,
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
    let mac: String? = macAddress.map { String(cString: $0) }

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
            commandLine: commandLine,
            readOnly: readOnly,
            enableNetworking: enableNetworking,
            macAddress: mac
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

@_cdecl("hb_vz_long_pause")
public func hb_vz_long_pause(
    vm: UnsafeMutablePointer<HbVzVm>?,
    outErr: UnsafeMutablePointer<UnsafeMutablePointer<CChar>?>?
) -> Int32 {
    guard let handle = borrowVz(vm, outErr) else { return Status.invalidArgument }
    do {
        try blockingPause(queue: handle.queue, holder: handle.holder)
        outErr?.pointee = nil
        return Status.ok
    } catch {
        writeError(outErr, formatError(error))
        return Status.swiftError
    }
}

/// Request a graceful guest shutdown — the VZ analog of Firecracker's
/// `SendCtrlAltDel`. `requestStop()` delivers an ACPI-style stop request;
/// the guest shuts down asynchronously. Returns an error if the VM can't
/// accept a stop request in its current state (e.g. not running).
@_cdecl("hb_vz_long_request_stop")
public func hb_vz_long_request_stop(
    vm: UnsafeMutablePointer<HbVzVm>?,
    outErr: UnsafeMutablePointer<UnsafeMutablePointer<CChar>?>?
) -> Int32 {
    guard let handle = borrowVz(vm, outErr) else { return Status.invalidArgument }
    let box = ErrorBox()
    var canStop = false
    handle.queue.sync {
        guard let vm = handle.holder.vm else { return }
        canStop = vm.canRequestStop
        guard canStop else { return }
        do { try vm.requestStop() } catch { box.value = error }
    }
    if !canStop {
        writeError(outErr, "VM cannot accept a stop request in its current state")
        return Status.swiftError
    }
    if let error = box.value {
        writeError(outErr, formatError(error))
        return Status.swiftError
    }
    outErr?.pointee = nil
    return Status.ok
}

@_cdecl("hb_vz_long_resume")
public func hb_vz_long_resume(
    vm: UnsafeMutablePointer<HbVzVm>?,
    outErr: UnsafeMutablePointer<UnsafeMutablePointer<CChar>?>?
) -> Int32 {
    guard let handle = borrowVz(vm, outErr) else { return Status.invalidArgument }
    do {
        try blockingResume(queue: handle.queue, holder: handle.holder)
        outErr?.pointee = nil
        return Status.ok
    } catch {
        writeError(outErr, formatError(error))
        return Status.swiftError
    }
}

@_cdecl("hb_vz_long_serve_mmds")
public func hb_vz_long_serve_mmds(
    vm: UnsafeMutablePointer<HbVzVm>?,
    port: UInt32,
    json: UnsafePointer<UInt8>?,
    jsonLen: Int,
    outErr: UnsafeMutablePointer<UnsafeMutablePointer<CChar>?>?
) -> Int32 {
    guard let vm, let json else {
        writeError(outErr, "null vm/json")
        return Status.invalidArgument
    }
    let handle = Unmanaged<VzVmHandle>.fromOpaque(UnsafeRawPointer(vm)).takeUnretainedValue()
    let data = Data(bytes: json, count: jsonLen)
    let sem = DispatchSemaphore(value: 0)
    let resultBox = ErrorBox()
    handle.queue.async {
        guard let vz = handle.holder.vm,
              let socketDevice = vz.socketDevices.first as? VZVirtioSocketDevice
        else {
            resultBox.value = VsockError.noSocketDevice
            sem.signal()
            return
        }
        let service = MmdsSocketService(json: data)
        socketDevice.setSocketListener(service.listener, forPort: port)
        handle.socketListeners.append(service)
        sem.signal()
    }
    sem.wait()
    if let error = resultBox.value {
        writeError(outErr, "\(error)")
        return Status.swiftError
    }
    return Status.ok
}

@_cdecl("hb_vz_long_connect")
public func hb_vz_long_connect(
    vm: UnsafeMutablePointer<HbVzVm>?,
    port: UInt32,
    outFd: UnsafeMutablePointer<Int32>?,
    outErr: UnsafeMutablePointer<UnsafeMutablePointer<CChar>?>?
) -> Int32 {
    guard let vm, let outFd else {
        writeError(outErr, "null vm/outFd")
        return Status.invalidArgument
    }
    let handle = Unmanaged<VzVmHandle>.fromOpaque(UnsafeRawPointer(vm)).takeUnretainedValue()
    let sem = DispatchSemaphore(value: 0)
    let resultBox = ConnectionResultBox()
    handle.queue.async {
        guard let vz = handle.holder.vm,
              let socketDevice = vz.socketDevices.first as? VZVirtioSocketDevice
        else {
            resultBox.result = .failure(VsockError.noSocketDevice)
            sem.signal()
            return
        }
        socketDevice.connect(toPort: port) { result in
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
    do {
        let conn: VZVirtioSocketConnection
        switch resultBox.result {
        case .success(let c): conn = c
        case .failure(let err): throw err
        case nil: throw VsockError.noSocketDevice
        }
        let fd = dup(conn.fileDescriptor)
        conn.close()
        if fd < 0 {
            throw POSIXError(.init(rawValue: errno) ?? .EIO)
        }
        outFd.pointee = fd
        return Status.ok
    } catch {
        writeError(outErr, "\(error)")
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

// =============================================================================
// FFI: hb_vz_pool_restore_long — restore a pre-warmed snapshot into a
// long-running VzVm handle (no command injection, no auto-stop).
//
// Counterpart to `hb_vz_long_new` + `hb_vz_long_start` for the cold-boot
// path: rebuilds the snapshot-compatible config, restores from `savePath`,
// resumes, and hands the caller a handle they pause/resume/stop/free with
// the existing `hb_vz_long_*` family.
//
// Critical: the config we build here MUST mirror what
// `ExecSession.makeSnapshotable` produced at save time, or VZ rejects
// the restore with "configuration mismatch". `outRestoreNanos` reports
// the wall time of restore+resume — the warm-start latency we'd want to
// surface in metrics later.
// =============================================================================

@_cdecl("hb_vz_pool_restore_long")
public func hb_vz_pool_restore_long(
    kernelPath: UnsafePointer<CChar>?,
    initramfsPath: UnsafePointer<CChar>?,
    rootfsPath: UnsafePointer<CChar>?,
    savePath: UnsafePointer<CChar>?,
    logPath: UnsafePointer<CChar>?,
    cpuCount: UInt32,
    memoryMib: UInt64,
    outVm: UnsafeMutablePointer<UnsafeMutablePointer<HbVzVm>?>?,
    outTimings: UnsafeMutablePointer<HbRestoreTimings>?,
    outErr: UnsafeMutablePointer<UnsafeMutablePointer<CChar>?>?
) -> Int32 {
    guard let kernelPath, let initramfsPath, let rootfsPath, let savePath, let outVm else {
        writeError(outErr, "null path/out argument")
        return Status.invalidArgument
    }
    let kernel = URL(fileURLWithPath: String(cString: kernelPath))
    let initramfs = URL(fileURLWithPath: String(cString: initramfsPath))
    let rootfs = URL(fileURLWithPath: String(cString: rootfsPath))
    let save = URL(fileURLWithPath: String(cString: savePath))
    let log: URL? = logPath.map { URL(fileURLWithPath: String(cString: $0)) }

    let machineId = machineIdURL(forSavePath: save)

    do {
        let configStart = DispatchTime.now()
        let session = try ExecSession.makeSnapshotable(
            kernel: kernel,
            initramfs: initramfs,
            rootfs: rootfs,
            logURL: log,
            machineIdURL: machineId,
            cpuCount: Int(cpuCount == 0 ? 2 : cpuCount),
            memoryBytes: (memoryMib == 0 ? 512 : memoryMib) * (1 << 20)
        )
        let configElapsed = DispatchTime.now().uptimeNanoseconds - configStart.uptimeNanoseconds
        // makeSnapshotable's session has no readabilityHandler / log handle
        // to keep alive; the URL-based serial attachment owns its file.

        let queue = DispatchQueue(label: "com.hephaestus.vz-pool-restore-\(UUID().uuidString)")
        let holder = VMHolder()
        let constructStart = DispatchTime.now()
        queue.sync {
            holder.vm = VZVirtualMachine(configuration: session.config, queue: queue)
        }
        let constructElapsed = DispatchTime.now().uptimeNanoseconds - constructStart.uptimeNanoseconds
        guard holder.vm != nil else {
            writeError(outErr, "failed to construct VZVirtualMachine")
            return Status.swiftError
        }

        let restoreStart = DispatchTime.now()
        try blockingRestore(queue: queue, holder: holder, from: save)
        let restoreElapsed = DispatchTime.now().uptimeNanoseconds - restoreStart.uptimeNanoseconds
        let resumeStart = DispatchTime.now()
        try blockingResume(queue: queue, holder: holder)
        let resumeElapsed = DispatchTime.now().uptimeNanoseconds - resumeStart.uptimeNanoseconds
        if let outTimings {
            outTimings.pointee = HbRestoreTimings(
                config_nanos: configElapsed,
                construct_nanos: constructElapsed,
                restore_nanos: restoreElapsed,
                resume_nanos: resumeElapsed
            )
        }

        // The agent inside is sitting at accept() on vsock 1234 forever —
        // we never connect, never send a command. Client treats this VM
        // as a generic running instance. See "agent-init divergence" in
        // docs/hephaestus-progress.md.
        let serialLog = log ?? URL(fileURLWithPath: "/dev/null")
        let handle = VzVmHandle(
            queue: queue,
            holder: holder,
            config: session.config,
            logURL: serialLog,
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

// =============================================================================
// FFI: hb_vz_stock_pool_restore_long — restore a stock-init snapshot (no
// agent, no vsock, no initramfs) into a long-running VzVm handle.
//
// Counterpart of `hb_vz_pool_restore_long` for `PoolFlavor::StockInit`.
// The save was produced by `hb_vz_snapshot_save`, so the restore config
// must mirror what `buildConfig` produces (URL serial, machine-id from
// the .machineid sidecar, the same `init=/bin/sh` cmdline). Anything
// else triggers VZ's "configuration mismatch" error on restore.
// =============================================================================

@_cdecl("hb_vz_stock_pool_restore_long")
public func hb_vz_stock_pool_restore_long(
    kernelPath: UnsafePointer<CChar>?,
    rootfsPath: UnsafePointer<CChar>?,
    savePath: UnsafePointer<CChar>?,
    logPath: UnsafePointer<CChar>?,
    cpuCount: UInt32,
    memoryMib: UInt64,
    outVm: UnsafeMutablePointer<UnsafeMutablePointer<HbVzVm>?>?,
    outTimings: UnsafeMutablePointer<HbRestoreTimings>?,
    outErr: UnsafeMutablePointer<UnsafeMutablePointer<CChar>?>?
) -> Int32 {
    guard let kernelPath, let rootfsPath, let savePath, let outVm else {
        writeError(outErr, "null path/out argument")
        return Status.invalidArgument
    }
    let kernel = URL(fileURLWithPath: String(cString: kernelPath))
    let rootfs = URL(fileURLWithPath: String(cString: rootfsPath))
    let save = URL(fileURLWithPath: String(cString: savePath))
    // logURL is required by buildConfig (the URL-based serial attachment
    // needs an existing file). When the caller doesn't pass one, drop it
    // next to the save file so it's discoverable post-mortem.
    let logURL = logPath.map { URL(fileURLWithPath: String(cString: $0)) }
        ?? save.deletingPathExtension().appendingPathExtension("restore.log")
    let machineId = machineIdURL(forSavePath: save)

    do {
        let configStart = DispatchTime.now()
        let config = try buildConfig(
            kernel: kernel,
            rootfs: rootfs,
            logURL: logURL,
            machineIdURL: machineId,
            cpuCount: Int(cpuCount == 0 ? 2 : cpuCount),
            memoryBytes: (memoryMib == 0 ? 512 : memoryMib) * (1 << 20),
            commandLine: "console=hvc0 root=/dev/vda rw init=/bin/sh panic=1"
        )
        let configElapsed = DispatchTime.now().uptimeNanoseconds - configStart.uptimeNanoseconds

        let queue = DispatchQueue(label: "com.hephaestus.vz-stock-pool-restore-\(UUID().uuidString)")
        let holder = VMHolder()
        let constructStart = DispatchTime.now()
        queue.sync {
            holder.vm = VZVirtualMachine(configuration: config, queue: queue)
        }
        let constructElapsed = DispatchTime.now().uptimeNanoseconds - constructStart.uptimeNanoseconds
        guard holder.vm != nil else {
            writeError(outErr, "failed to construct VZVirtualMachine")
            return Status.swiftError
        }

        let restoreStart = DispatchTime.now()
        try blockingRestore(queue: queue, holder: holder, from: save)
        let restoreElapsed = DispatchTime.now().uptimeNanoseconds - restoreStart.uptimeNanoseconds
        let resumeStart = DispatchTime.now()
        try blockingResume(queue: queue, holder: holder)
        let resumeElapsed = DispatchTime.now().uptimeNanoseconds - resumeStart.uptimeNanoseconds
        if let outTimings {
            outTimings.pointee = HbRestoreTimings(
                config_nanos: configElapsed,
                construct_nanos: constructElapsed,
                restore_nanos: restoreElapsed,
                resume_nanos: resumeElapsed
            )
        }

        let handle = VzVmHandle(
            queue: queue,
            holder: holder,
            config: config,
            logURL: logURL,
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

// =============================================================================
// FFI: hb_vz_long_save — save the state of a long-running VzVm handle
// to disk. Pairs with hb_vz_long_restore. The VM must already be Paused
// (VZ's saveMachineStateTo: requires it); the caller arranges that via
// hb_vz_long_pause first. This FFI does not pause/resume on the
// caller's behalf — staying out of the state machine keeps the
// PUT /snapshot/create contract clean.
//
// Also writes the platform machine identifier next to the save file
// (`<save>.machineid`) so a restore in a fresh process can recreate
// the same VZGenericMachineIdentifier.
// =============================================================================

@_cdecl("hb_vz_long_save")
public func hb_vz_long_save(
    vm: UnsafeMutablePointer<HbVzVm>?,
    savePath: UnsafePointer<CChar>?,
    outErr: UnsafeMutablePointer<UnsafeMutablePointer<CChar>?>?
) -> Int32 {
    guard let handle = borrowVz(vm, outErr) else { return Status.invalidArgument }
    guard let savePath else {
        writeError(outErr, "null savePath")
        return Status.invalidArgument
    }
    let save = URL(fileURLWithPath: String(cString: savePath))

    // Persist the machine-id sidecar so a future restore in a different
    // process can rebuild a config with the same VZGenericMachineIdentifier.
    let outIdURL = machineIdURL(forSavePath: save)
    if outIdURL != handle.machineIdURL,
       let data = try? Data(contentsOf: handle.machineIdURL) {
        try? data.write(to: outIdURL)
    }

    do {
        try blockingSave(queue: handle.queue, holder: handle.holder, to: save)
        outErr?.pointee = nil
        return Status.ok
    } catch {
        writeError(outErr, formatError(error))
        return Status.swiftError
    }
}

// =============================================================================
// FFI: hb_vz_long_restore — restore a snapshot taken by hb_vz_long_save
// into a fresh long-running VzVm handle.
//
// Caller supplies the same kernel/rootfs/cmdline/cpu/mem the original
// VM was created with (via PUT /machine-config + PUT /boot-source +
// PUT /drives, exactly the upstream `PUT /snapshot/load` flow). Config
// must mirror `buildLongRunningConfig` since that's what produced the
// VM that was saved.
//
// `resume` controls whether the restored VM resumes immediately (true)
// or stays paused (false). Either way the caller gets a handle they
// can pause/resume/stop/free.
// =============================================================================

@_cdecl("hb_vz_long_restore")
public func hb_vz_long_restore(
    kernelPath: UnsafePointer<CChar>?,
    rootfsPath: UnsafePointer<CChar>?,
    initrdPath: UnsafePointer<CChar>?,
    logPath: UnsafePointer<CChar>?,
    bootArgs: UnsafePointer<CChar>?,
    savePath: UnsafePointer<CChar>?,
    cpuCount: UInt32,
    memoryMib: UInt64,
    readOnly: Bool,
    enableNetworking: Bool,
    macAddress: UnsafePointer<CChar>?,
    resume: Bool,
    outVm: UnsafeMutablePointer<UnsafeMutablePointer<HbVzVm>?>?,
    outTimings: UnsafeMutablePointer<HbRestoreTimings>?,
    outErr: UnsafeMutablePointer<UnsafeMutablePointer<CChar>?>?
) -> Int32 {
    guard let kernelPath, let rootfsPath, let logPath, let bootArgs, let savePath, let outVm
    else {
        writeError(outErr, "null path/out argument")
        return Status.invalidArgument
    }

    let kernel = URL(fileURLWithPath: String(cString: kernelPath))
    let rootfs = URL(fileURLWithPath: String(cString: rootfsPath))
    let log = URL(fileURLWithPath: String(cString: logPath))
    let initrd: URL? = initrdPath.map { URL(fileURLWithPath: String(cString: $0)) }
    let commandLine = String(cString: bootArgs)
    let save = URL(fileURLWithPath: String(cString: savePath))
    let machineId = machineIdURL(forSavePath: save)
    let mac: String? = macAddress.map { String(cString: $0) }

    do {
        let configStart = DispatchTime.now()
        let config = try buildLongRunningConfig(
            kernel: kernel,
            rootfs: rootfs,
            initrd: initrd,
            logURL: log,
            machineIdURL: machineId,
            cpuCount: Int(cpuCount == 0 ? 2 : cpuCount),
            memoryBytes: (memoryMib == 0 ? 512 : memoryMib) * (1 << 20),
            commandLine: commandLine,
            readOnly: readOnly,
            enableNetworking: enableNetworking,
            macAddress: mac
        )
        let configElapsed = DispatchTime.now().uptimeNanoseconds - configStart.uptimeNanoseconds

        let queue = DispatchQueue(label: "com.hephaestus.vz-long-restore-\(UUID().uuidString)")
        let holder = VMHolder()
        let constructStart = DispatchTime.now()
        queue.sync {
            holder.vm = VZVirtualMachine(configuration: config, queue: queue)
        }
        let constructElapsed = DispatchTime.now().uptimeNanoseconds - constructStart.uptimeNanoseconds
        guard holder.vm != nil else {
            writeError(outErr, "failed to construct VZVirtualMachine")
            return Status.swiftError
        }

        let restoreStart = DispatchTime.now()
        try blockingRestore(queue: queue, holder: holder, from: save)
        let restoreElapsed = DispatchTime.now().uptimeNanoseconds - restoreStart.uptimeNanoseconds
        var resumeElapsed: UInt64 = 0
        if resume {
            let resumeStart = DispatchTime.now()
            try blockingResume(queue: queue, holder: holder)
            resumeElapsed = DispatchTime.now().uptimeNanoseconds - resumeStart.uptimeNanoseconds
        }
        if let outTimings {
            outTimings.pointee = HbRestoreTimings(
                config_nanos: configElapsed,
                construct_nanos: constructElapsed,
                restore_nanos: restoreElapsed,
                resume_nanos: resumeElapsed
            )
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
