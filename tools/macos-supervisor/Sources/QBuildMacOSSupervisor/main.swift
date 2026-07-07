import ArgumentParser
import Containerization
import ContainerizationArchive
import ContainerizationEXT4
import ContainerizationOCI
#if canImport(CryptoKit)
import CryptoKit
#endif
import Foundation
import Logging
import SystemPackage

@main
struct QBuildMacOSSupervisor: AsyncParsableCommand {
    static let configuration = CommandConfiguration(
        commandName: "qbuild-macos-supervisor",
        abstract: "Persistent Linux guest supervisor for qbuild on macOS",
        subcommands: [Daemon.self, BuildAssets.self]
    )
}

extension QBuildMacOSSupervisor {
    struct Daemon: AsyncParsableCommand {
        @Option(name: .long)
        var stateDir: String

        @Option(name: .long)
        var guestSocketHost: String

        @Option(name: .long)
        var guestRootfs: String

        @Option(name: .long)
        var initBlock: String

        @Option(name: .long)
        var kernel: String

        @Option(name: .long)
        var homeShare: String

        @Option(name: .long)
        var cpus: Int = 4

        @Option(name: .long)
        var memoryMiB: UInt64 = 4096

        @Flag(name: .long)
        var rosetta = false

        func run() async throws {
            LoggingSystem.bootstrap(StreamLogHandler.standardError)
            var logger = Logger(label: "qbuild.macos.supervisor")
            logger.logLevel = .info

            let stateURL = URL(fileURLWithPath: stateDir, isDirectory: true)
            try FileManager.default.createDirectory(at: stateURL, withIntermediateDirectories: true)

            let guestSocketHostURL = URL(fileURLWithPath: guestSocketHost)
            try? FileManager.default.removeItem(at: guestSocketHostURL)

            let rootfs = Mount.block(
                format: "ext4",
                source: guestRootfs,
                destination: "/",
                options: []
            )
            let initfs = Mount.block(
                format: "ext4",
                source: initBlock,
                destination: "/",
                options: ["ro"]
            )

            var guestKernel = Kernel(path: .init(filePath: kernel), platform: .linuxArm)
            guestKernel.commandLine.addDebug()

            let vmm = VZVirtualMachineManager(
                kernel: guestKernel,
                initialFilesystem: initfs,
                bootlog: stateURL.appendingPathComponent("boot.log").path,
                logger: logger
            )

            let container = LinuxContainer("qbuild-guest", rootfs: rootfs, vmm: vmm, logger: logger)
            container.cpus = cpus
            container.memoryInBytes = memoryMiB * 1024 * 1024
            container.rosetta = rosetta
            container.arguments = ["/usr/local/bin/qbuild", "guestd", "--listen-unix", "/run/qbuild/guestd.sock"]
            container.mounts.append(
                .share(
                    source: homeShare,
                    destination: homeShare,
                    options: []
                )
            )
            container.sockets = [
                UnixSocketConfiguration(
                    source: URL(fileURLWithPath: "/run/qbuild/guestd.sock"),
                    destination: guestSocketHostURL,
                    direction: .outOf
                )
            ]

            logger.info("creating qbuild guest")
            try await container.create()
            logger.info("starting qbuild guest daemon")
            try await container.start()

            let pidPath = stateURL.appendingPathComponent("supervisor.pid")
            try "\(ProcessInfo.processInfo.processIdentifier)\n".write(to: pidPath, atomically: true, encoding: .utf8)

            _ = try await container.wait()
        }
    }

    struct BuildAssets: AsyncParsableCommand {
        @Option(name: .long)
        var outputDir: String

        @Option(name: .long)
        var qbuildLinuxBinary: String

        @Option(name: .long)
        var vminitd: String

        @Option(name: .long)
        var vmexec: String

        @Option(name: .long)
        var kernel: String

        @Option(name: .long)
        var baseImage: String = "docker.io/library/alpine:3.20"

        @Option(name: .long)
        var baseImagePlatform: String = "linux/arm64/v8"

        @Option(name: .long)
        var rootfsSizeMiB: UInt64 = 4096

        @Option(name: .long)
        var initImageRef: String = "qbuild.local/init:latest"

        @Option(name: .long)
        var manifestVersion: Int = 1

        func run() async throws {
            LoggingSystem.bootstrap(StreamLogHandler.standardError)
            var logger = Logger(label: "qbuild.macos.assets")
            logger.logLevel = .info

            let outputURL = URL(fileURLWithPath: outputDir, isDirectory: true)
            try FileManager.default.createDirectory(at: outputURL, withIntermediateDirectories: true)

            let bundle = BundlePaths(outputURL: outputURL)
            let scratchURL = outputURL.appendingPathComponent(".build-assets", isDirectory: true)
            try? FileManager.default.removeItem(at: scratchURL)
            try FileManager.default.createDirectory(at: scratchURL, withIntermediateDirectories: true)

            defer {
                try? FileManager.default.removeItem(at: scratchURL)
            }

            let storeRoot = scratchURL.appendingPathComponent("image-store", isDirectory: true)
            let contentRoot = storeRoot.appendingPathComponent("content", isDirectory: true)
            let contentStore = try LocalContentStore(path: contentRoot)
            let imageStore = try ImageStore(path: storeRoot, contentStore: contentStore)

            let platform = try Platform(from: baseImagePlatform)
            logger.info("creating init archive")
            let initArchive = scratchURL.appendingPathComponent("init-rootfs.tar.gz")
            try Self.writeInitArchive(
                initArchive: initArchive,
                vminitd: URL(fileURLWithPath: vminitd),
                vmexec: URL(fileURLWithPath: vmexec)
            )

            logger.info("assembling init.block")
            let initImage = try await InitImage.create(
                reference: initImageRef,
                rootfs: initArchive,
                platform: platform,
                imageStore: imageStore,
                contentStore: contentStore
            )
            _ = try await initImage.initBlock(at: bundle.initBlockURL, for: .linuxArm)

            logger.info("pulling base image \(baseImage)")
            let image = try await imageStore.pull(reference: baseImage, platform: platform)

            let overlayArchive = scratchURL.appendingPathComponent("guest-overlay.tar.gz")
            logger.info("creating guest overlay archive")
            try Self.writeGuestOverlayArchive(
                overlayArchive: overlayArchive,
                qbuildBinary: URL(fileURLWithPath: qbuildLinuxBinary)
            )

            logger.info("assembling guest-rootfs.ext4")
            try await Self.buildGuestRootfs(
                image: image,
                platform: platform,
                outputURL: bundle.guestRootfsURL,
                overlayArchive: overlayArchive,
                minDiskSizeBytes: rootfsSizeMiB * 1024 * 1024
            )

            logger.info("copying kernel")
            try FileManager.default.copyItem(at: URL(fileURLWithPath: kernel), to: bundle.kernelURL)

            logger.info("writing manifest")
            let manifest = try Self.makeManifest(
                manifestVersion: manifestVersion,
                platform: baseImagePlatform,
                kernelURL: bundle.kernelURL,
                initBlockURL: bundle.initBlockURL,
                guestRootfsURL: bundle.guestRootfsURL
            )
            let manifestData = try JSONEncoder.pretty.encode(manifest)
            try manifestData.write(to: bundle.manifestURL)

            logger.info("guest asset bundle ready at \(outputURL.path)")
        }
    }
}

private struct BundlePaths {
    let outputURL: URL

    var kernelURL: URL { outputURL.appendingPathComponent("vmlinux") }
    var initBlockURL: URL { outputURL.appendingPathComponent("init.block") }
    var guestRootfsURL: URL { outputURL.appendingPathComponent("guest-rootfs.ext4") }
    var manifestURL: URL { outputURL.appendingPathComponent("manifest.json") }
}

private struct GuestAssetManifest: Codable {
    struct Artifact: Codable {
        let file: String
        let sha256: String
        let sizeBytes: UInt64
    }

    let version: Int
    let platform: String
    let kernel: Artifact
    let initBlock: Artifact
    let guestRootfs: Artifact
    let guestdSocketPath: String
    let guestdBinaryPath: String
}

private extension QBuildMacOSSupervisor.BuildAssets {
    static func writeInitArchive(initArchive: URL, vminitd: URL, vmexec: URL) throws {
        let writer = try ArchiveWriter(format: .pax, filter: .gzip, file: initArchive)
        let ts = Date()
        let entry = WriteEntry()
        entry.permissions = 0o755
        entry.modificationDate = ts
        entry.creationDate = ts
        entry.group = 0
        entry.owner = 0
        entry.fileType = .directory

        for dir in ["bin", "sbin", "dev", "sys", "proc/self", "run", "tmp", "mnt", "var"] {
            entry.path = dir
            try writer.writeEntry(entry: entry, data: nil)
        }

        entry.fileType = .regular
        entry.path = "sbin/vminitd"
        var data = try Data(contentsOf: vminitd)
        entry.size = Int64(data.count)
        try writer.writeEntry(entry: entry, data: data)

        entry.path = "sbin/vmexec"
        data = try Data(contentsOf: vmexec)
        entry.size = Int64(data.count)
        try writer.writeEntry(entry: entry, data: data)

        entry.fileType = .symbolicLink
        entry.path = "proc/self/exe"
        entry.symlinkTarget = "sbin/vminitd"
        entry.size = nil
        try writer.writeEntry(entry: entry, data: nil)
        try writer.finishEncoding()
    }

    static func writeGuestOverlayArchive(overlayArchive: URL, qbuildBinary: URL) throws {
        let writer = try ArchiveWriter(format: .pax, filter: .gzip, file: overlayArchive)
        let ts = Date()
        let entry = WriteEntry()
        entry.permissions = 0o755
        entry.modificationDate = ts
        entry.creationDate = ts
        entry.group = 0
        entry.owner = 0
        entry.fileType = .directory

        for dir in [
            "usr",
            "usr/local",
            "usr/local/bin",
            "run",
            "run/qbuild",
            "var",
            "var/lib",
            "var/lib/qbuild",
            "var/lib/qbuild/images",
            "var/lib/qbuild/builds",
            "var/lib/qbuild/containers",
            "etc",
        ] {
            entry.path = dir
            try writer.writeEntry(entry: entry, data: nil)
        }

        entry.fileType = .regular
        entry.path = "usr/local/bin/qbuild"
        var data = try Data(contentsOf: qbuildBinary)
        entry.size = Int64(data.count)
        try writer.writeEntry(entry: entry, data: data)

        entry.path = "etc/qbuild-guest-release"
        entry.permissions = 0o644
        data = Data("QBUILD_GUEST=1\n".utf8)
        entry.size = Int64(data.count)
        try writer.writeEntry(entry: entry, data: data)

        try writer.finishEncoding()
    }

    static func buildGuestRootfs(
        image: Containerization.Image,
        platform: Platform,
        outputURL: URL,
        overlayArchive: URL,
        minDiskSizeBytes: UInt64
    ) async throws {
        if FileManager.default.fileExists(atPath: outputURL.path) {
            try FileManager.default.removeItem(at: outputURL)
        }

        let manifest = try await image.manifest(for: platform)
        let formatter = try EXT4.Formatter(FilePath(outputURL.path), minDiskSize: minDiskSizeBytes)
        defer { try? formatter.close() }

        for layer in manifest.layers {
            let content = try await image.getContent(digest: layer.digest)
            switch layer.mediaType {
            case MediaTypes.imageLayer, MediaTypes.dockerImageLayer:
                try formatter.unpack(
                    source: content.path,
                    format: .paxRestricted,
                    compression: .none
                )
            case MediaTypes.imageLayerGzip, MediaTypes.dockerImageLayerGzip:
                try formatter.unpack(
                    source: content.path,
                    format: .paxRestricted,
                    compression: .gzip
                )
            default:
                throw ValidationError("Unsupported base image layer media type \(layer.mediaType)")
            }
        }

        try formatter.unpack(
            source: overlayArchive,
            format: .paxRestricted,
            compression: .gzip
        )
    }

    static func makeManifest(
        manifestVersion: Int,
        platform: String,
        kernelURL: URL,
        initBlockURL: URL,
        guestRootfsURL: URL
    ) throws -> GuestAssetManifest {
        GuestAssetManifest(
            version: manifestVersion,
            platform: platform,
            kernel: try artifact(for: kernelURL),
            initBlock: try artifact(for: initBlockURL),
            guestRootfs: try artifact(for: guestRootfsURL),
            guestdSocketPath: "/run/qbuild/guestd.sock",
            guestdBinaryPath: "/usr/local/bin/qbuild"
        )
    }

    static func artifact(for url: URL) throws -> GuestAssetManifest.Artifact {
        let data = try Data(contentsOf: url)
        return .init(
            file: url.lastPathComponent,
            sha256: SHA256.hexDigest(for: data),
            sizeBytes: UInt64(data.count)
        )
    }
}

private enum SHA256 {
    static func hexDigest(for data: Data) -> String {
        #if canImport(CryptoKit)
        let digest = CryptoKit.SHA256.hash(data: data)
        return digest.map { String(format: "%02x", $0) }.joined()
        #else
        fatalError("CryptoKit unavailable")
        #endif
    }
}

private extension JSONEncoder {
    static var pretty: JSONEncoder {
        let encoder = JSONEncoder()
        encoder.outputFormatting = [.prettyPrinted, .sortedKeys]
        return encoder
    }
}
