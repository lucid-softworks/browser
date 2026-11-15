#!/usr/bin/env python3
"""Write Finder's installer window directly, without Apple Events or Finder automation.

Run on the mounted writable image, after creating .background/background.png.
The Carbon alias records this volume's real identity and survives UDZO conversion.
Dependencies are pinned in macos-dmg-requirements.txt.
"""
import argparse
from pathlib import Path

from ds_store import DSStore
from mac_alias import Alias

WINDOW_BOUNDS = "{{200, 160}, {640, 420}}"
ICON_POSITIONS = {"Browser.app": (170, 205), "Applications": (470, 205)}


def write_layout(volume: Path, background_alias: bytes) -> None:
    with DSStore.open(str(volume / ".DS_Store"), "w+") as layout:
        layout["."]["vSrn"] = ("long", 1)
        layout["."]["icvl"] = ("type", b"icnv")
        layout["."]["bwsp"] = {
            "WindowBounds": WINDOW_BOUNDS,
            "ShowToolbar": False,
            "ShowStatusBar": False,
            "ShowSidebar": False,
            "ContainerShowSidebar": False,
            "ShowPathbar": False,
            "ShowTabView": False,
            "PreviewPaneVisibility": False,
        }
        layout["."]["icvp"] = {
            "viewOptionsVersion": 1,
            "backgroundType": 2,
            "backgroundImageAlias": background_alias,
            "iconSize": 96.0,
            "textSize": 13.0,
            "labelOnBottom": True,
            "showItemInfo": False,
            "showIconPreview": True,
            "arrangeBy": "none",
            "gridOffsetX": 0.0,
            "gridOffsetY": 0.0,
            "gridSpacing": 100.0,
        }
        for name, position in ICON_POSITIONS.items():
            layout[name]["Iloc"] = position


def verify_layout(volume: Path, expected_alias=None) -> None:
    with DSStore.open(str(volume / ".DS_Store"), "r") as layout:
        assert layout["."]["bwsp"]["WindowBounds"] == WINDOW_BOUNDS
        assert layout["."]["icvp"]["backgroundImageAlias"]
        if expected_alias is not None:
            assert layout["."]["icvp"]["backgroundImageAlias"] == expected_alias
        assert layout["."]["icvp"]["arrangeBy"] == "none"
        for name, position in ICON_POSITIONS.items():
            assert tuple(layout[name]["Iloc"]) == position


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("volume", type=Path, help="Mounted writable DMG root")
    parser.add_argument("--verify-only", action="store_true", help="Check a finished read-only image")
    args = parser.parse_args()
    volume = args.volume.resolve(strict=True)
    background = volume / ".background" / "background.png"
    if not (volume / "Browser.app").is_dir() or not (volume / "Applications").is_symlink():
        parser.error("The mounted image must contain Browser.app and an Applications shortcut")
    if not background.is_file():
        parser.error("Missing .background/background.png")
    if args.verify_only:
        verify_layout(volume)
    else:
        alias = Alias.for_file(str(background)).to_bytes()
        write_layout(volume, alias)
        verify_layout(volume, alias)
    print(f"Verified installer layout in {volume}")


if __name__ == "__main__":
    main()
