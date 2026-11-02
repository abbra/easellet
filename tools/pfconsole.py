#!/usr/bin/env python3
"""Send console commands to the photoframe USB serial port and print replies."""
import sys
import time

import serial

PORT = "/dev/ttyACM0"
# A reply is one reply, but `ai selftest` and `config` answer with several lines, so the
# read ends when the line has been quiet for this long rather than at the first newline.
QUIET_S = 0.6


def main() -> int:
    commands = sys.argv[1:]
    with serial.Serial(PORT, 115200, timeout=0.2) as port:
        # DTR/RTS are left alone on purpose. The S3's USB-Serial-JTAG block treats
        # a DTR/RTS edge as a reset request, so toggling them here restarted the
        # frame on every probe: boot output appeared as a fake reply and the idle
        # watcher's counters looked freshly reset. Opening the port does not reset.
        time.sleep(0.2)
        port.reset_input_buffer()
        for command in commands:
            port.write(command.encode() + b"\r")
            port.flush()
            reply = b""
            deadline = time.time() + 120
            last_data = 0.0
            while time.time() < deadline:
                chunk = port.read(4096)
                if chunk:
                    reply += chunk
                    last_data = time.time()
                elif reply and time.time() - last_data > QUIET_S:
                    break
                else:
                    time.sleep(0.02)
            text = reply.decode(errors="replace").strip()
            print(f"$ {command}")
            print(text if text else "<no reply>")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
