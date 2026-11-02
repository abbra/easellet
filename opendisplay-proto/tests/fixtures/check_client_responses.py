#!/usr/bin/env python3
"""Check the Rust server's responses with py-opendisplay's own parsers.

tests/client.rs writes every response it produced as "label hex" lines. This
script runs them through the client's response code and fails on anything the
client would reject. Usage:

    .tools/venv-opendisplay/bin/python check_client_responses.py RESPONSES
"""

import sys
from collections import defaultdict

from opendisplay.protocol.config_parser import parse_config_response
from opendisplay.protocol.responses import (
    check_response_type,
    parse_firmware_version,
    parse_read_msd,
    strip_command_echo,
    validate_ack_response,
)
from opendisplay.protocol import CommandCode as Cmd


def main(path: str) -> None:
    frames: dict[str, list[bytes]] = defaultdict(list)
    with open(path) as responses:
        for line in responses:
            label, data = line.split()
            frames[label].append(bytes.fromhex(data))

    version = parse_firmware_version(frames["fw_version"][0])
    assert (version["major"], version["minor"], version["patch"]) == (1, 2, 3), version
    assert version["sha"] == "abc1234", version

    assert parse_read_msd(frames["read_msd"][0]) == bytes(range(16))

    # interrogate(): chunk 0 carries the total length, later chunks a number.
    first, *rest = (strip_command_echo(frame, Cmd.READ_CONFIG) for frame in frames["read_config"])
    total = int.from_bytes(first[2:4], "little")
    tlv = bytearray(first[4:])
    for chunk in rest:
        tlv.extend(chunk[2:])
    assert len(tlv) == total, (len(tlv), total)
    config = parse_config_response(bytes(tlv))
    display = config.displays[0]
    assert (display.pixel_width, display.pixel_height, display.rotation) == (800, 480, 2)

    for label, command in [
        ("config_write", Cmd.WRITE_CONFIG),
        ("config_chunk", Cmd.WRITE_CONFIG_CHUNK),
        ("lan_start", Cmd.DIRECT_WRITE_START),
        ("lan_data", Cmd.DIRECT_WRITE_DATA),
        ("lan_end", Cmd.DIRECT_WRITE_END),
        ("ble_start", Cmd.DIRECT_WRITE_START),
        ("ble_data", Cmd.DIRECT_WRITE_DATA),
        ("ble_end", Cmd.DIRECT_WRITE_END),
    ]:
        assert frames[label], label
        for frame in frames[label]:
            validate_ack_response(frame, command)

    command, _ = check_response_type(frames["refresh"][0])
    assert command == Cmd.DIRECT_WRITE_REFRESH_COMPLETE, command
    command, _ = check_response_type(frames["refresh_timeout"][0])
    assert command == Cmd.DIRECT_WRITE_REFRESH_TIMEOUT, command

    print(f"client accepted {sum(len(v) for v in frames.values())} responses")


if __name__ == "__main__":
    main(sys.argv[1])
