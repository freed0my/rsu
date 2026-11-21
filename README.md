# rsu!

rsu! is an custom client for Linux, written in Rust.
It focuses on low latency and uses a custom renderer. Maybe will support windows later idk

It's still early in development. Expect bugs, missing features, and things that don't quite work as they should.
Already supports watching replays from stable and parsing your maps from it.

## Build and run

On Arch Linux:

    sudo pacman -S --needed rust clang pkgconf pipewire wayland libxcb vulkan-icd-loader
    cargo build --release
    target/release/rsu

You'll also need a Vulkan 1.3 compatible GPU driver. Run `target/release/rsu --help` for more options.

## Credits

- McOsu — inspiration for the UI/UX.
- Rewind — inspiration for the replay parser implementation.

## License

GPL-3.0-only. See [LICENSE](LICENSE).
