#!/usr/bin/env python3
"""Check the frame's MSD record and TXT value with py-opendisplay's own decoders.

tests/client.rs writes one record with known values; this parses it as Home
Assistant and the CLI do: the advertisement decoder for battery, chip
temperature and flags, and `read_sensor_values` with the board config, which is
how a client finds the ambient sensor's slot. Usage:

    .tools/venv-opendisplay/bin/python check_msd.py RECORD_FILE BOARD_CONFIG
"""

import sys
from pathlib import Path

from opendisplay.models.advertisement import parse_advertisement
from opendisplay.protocol.config_parser import parse_config_response
from opendisplay.sensors import read_sensor_values


def main(record_path: str, config_path: str) -> None:
    lines = dict(line.split() for line in Path(record_path).read_text().splitlines())
    record = bytes.fromhex(lines["record"])
    advertisement = parse_advertisement(record)
    assert advertisement.format_version == "v1", advertisement
    assert advertisement.battery_mv == 4170, advertisement.battery_mv
    assert advertisement.temperature_c == 31.0, advertisement.temperature_c
    assert advertisement.reboot_flag is True, advertisement
    assert advertisement.loop_counter == 5, advertisement.loop_counter

    config = parse_config_response(Path(config_path).read_bytes())
    readings = read_sensor_values(config, advertisement)
    assert len(readings) == 1, readings
    (ambient,) = readings
    assert round(ambient.temperature_c, 1) == -5.5, ambient
    assert round(ambient.humidity_percent, 1) == 33.5, ambient
    assert config.power.power_mode == 1, config.power

    # The TXT value is the same record without the company id.
    assert bytes.fromhex(lines["txt"]) == record[2:], lines["txt"]
    assert len(lines["txt"]) == 28
    print("py-opendisplay decoded the MSD: battery, chip temperature, flags, ambient sensor")


if __name__ == "__main__":
    main(sys.argv[1], sys.argv[2])
