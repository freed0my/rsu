# rsu!

rsu! is an osu!standard client for Linux, written in Rust.

It's still early in development. Expect bugs, missing features, and things that don't quite work as they should.

## Build and run

On Arch Linux:

    sudo pacman -S --needed rust clang pkgconf pipewire wayland libxcb vulkan-icd-loader
    cargo build --release
    target/release/rsu

You'll also need a Vulkan 1.3 compatible GPU driver. Run `target/release/rsu --help` for more options.

## License

GPL-3.0-only. See [LICENSE](LICENSE).
