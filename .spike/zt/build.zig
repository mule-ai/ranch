const std = @import("std");

pub fn build(b: *std.Build) void {
    const target = b.standardTargetOptions(.{});
    const optimize = b.standardOptimizeOption(.{});

    if (b.lazyDependency("ghostty", .{})) |dep| {
        // Build the libghostty-vt shared and static artifacts from the pinned
        // ghostty source and install them into zig-out/lib so the Rust spike
        // can link against them.
        const vt_shared = dep.artifact("ghostty-vt");
        vt_shared.install(b.getInstallStep());
    }
}
