# ESP32-S3 debugging

This uses the Rust on ESP Book's Cortex-Debug + Espressif OpenOCD + Xtensa GDB
workflow for the project's `xtensa-esp32s3-espidf` target and Seeed XIAO
ESP32S3 Sense. Run VS Code in the `esp32-rust` devcontainer; it installs
Cortex-Debug and provides the configured tools.

Set a breakpoint in `firmware/src/main.rs`, then press F5 to build, flash and
verify the bootloader/partition table/application over USB-JTAG, and attach
Xtensa GDB while the chip remains halted. Cortex-Debug resolves breakpoints
before Continue; its reset command keeps the target halted. The flash task
uses `scripts/flash-debug-jtag.sh` to find the current ESP-IDF build directory,
avoiding hard-coded build hashes. This does not claim `/dev/ttyACM0`, so an
existing serial monitor can stay open.

The `ESP32-S3: Attach without reset` profile is for attaching to an already
running image when a reset triggers early PSRAM initialization problems. It
builds symbols but does not flash/reset; set breakpoints before or immediately
after attaching and use Pause/Continue in the debugger.

OpenOCD uses Espressif's `board/esp32s3-builtin.cfg`, which selects the board's
built-in USB-JTAG adapter. It must identify the USB device and report successful
examination of `esp32s3.cpu0` and `esp32s3.cpu1`. If it cannot see the JTAG
device, reconnect the board's native USB port and verify the USB device is
passed through to the devcontainer. An external JTAG probe needs a different
OpenOCD configuration.

The debug configuration starts at the program entry. The current firmware
`main()` is still a camera-bindings smoke test; it is not yet the full device
boot sequence.
