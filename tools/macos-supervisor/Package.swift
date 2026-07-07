// swift-tools-version: 6.1

import PackageDescription

let package = Package(
    name: "qbuild-macos-supervisor",
    platforms: [.macOS("15")],
    products: [
        .executable(name: "qbuild-macos-supervisor", targets: ["QBuildMacOSSupervisor"]),
    ],
    dependencies: [
        .package(url: "https://github.com/swift-server/async-http-client.git", exact: "1.26.1"),
        .package(url: "https://github.com/apple/swift-argument-parser.git", exact: "1.5.1"),
        .package(url: "https://github.com/grpc/grpc-swift.git", exact: "1.26.1"),
        .package(url: "https://github.com/apple/swift-algorithms.git", exact: "1.2.1"),
        .package(url: "https://github.com/apple/swift-asn1.git", exact: "1.3.2"),
        .package(url: "https://github.com/apple/swift-async-algorithms.git", exact: "1.0.4"),
        .package(url: "https://github.com/apple/swift-atomics.git", exact: "1.2.0"),
        .package(url: "https://github.com/apple/swift-certificates.git", exact: "1.10.0"),
        .package(url: "https://github.com/apple/swift-collections.git", exact: "1.2.0"),
        .package(url: "https://github.com/apple/swift-log.git", exact: "1.6.3"),
        .package(url: "https://github.com/apple/swift-crypto.git", exact: "3.12.3"),
        .package(url: "https://github.com/apple/swift-http-structured-headers.git", exact: "1.3.0"),
        .package(url: "https://github.com/apple/swift-http-types.git", exact: "1.4.0"),
        .package(url: "https://github.com/apple/swift-nio.git", exact: "2.83.0"),
        .package(url: "https://github.com/apple/swift-nio-extras.git", exact: "1.28.0"),
        .package(url: "https://github.com/apple/swift-nio-http2.git", exact: "1.36.0"),
        .package(url: "https://github.com/apple/swift-nio-ssl.git", exact: "2.31.0"),
        .package(url: "https://github.com/apple/swift-nio-transport-services.git", exact: "1.24.0"),
        .package(url: "https://github.com/apple/swift-numerics.git", exact: "1.0.3"),
        .package(url: "https://github.com/apple/swift-protobuf.git", exact: "1.30.0"),
        .package(url: "https://github.com/swift-server/swift-service-lifecycle.git", exact: "2.8.0"),
        .package(url: "https://github.com/swiftlang/swift-syntax.git", exact: "600.0.1"),
        .package(url: "https://github.com/apple/swift-system.git", exact: "1.5.0"),
        .package(url: "https://github.com/apple/containerization.git", exact: "0.1.1"),
    ],
    targets: [
        .executableTarget(
            name: "QBuildMacOSSupervisor",
            dependencies: [
                .product(name: "ArgumentParser", package: "swift-argument-parser"),
                .product(name: "Logging", package: "swift-log"),
                .product(name: "Containerization", package: "containerization"),
                .product(name: "ContainerizationArchive", package: "containerization"),
                .product(name: "ContainerizationEXT4", package: "containerization"),
                .product(name: "ContainerizationOCI", package: "containerization"),
                .product(name: "SystemPackage", package: "swift-system"),
            ],
            swiftSettings: [
                .define("CURRENT_SDK")
            ]
        ),
    ]
)
