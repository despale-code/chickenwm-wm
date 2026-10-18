<img src="icon.png" width="96" align="right" alt="">

# ChickenWM WM

The tiling window manager: dwindle and master layouts, workspaces 1–10, gaps, borders, rounded corners, animations and window rules.

Part of [ChickenWM](https://github.com/despale-code/chickenwm), a Hyprland-style tiling desktop for
Windows. Each program lives in its own repository; the ChickenWM repository ties them together
and has the full documentation and the installer.

This project is vibecoded: written with an AI assistant.

## Building

Windows 10 or 11 (64-bit) and Rust 1.85 or newer:

```
cargo build --release
```

It depends on [chickenwm-core](https://github.com/despale-code/chickenwm-core) and [chickenwm-dwm-hook](https://github.com/despale-code/chickenwm-dwm-hook), which Cargo fetches from GitHub.

## License

MIT, see [LICENSE](LICENSE).
