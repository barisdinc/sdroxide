#!/usr/bin/env python3
"""Draw the packaged APRS station icons.

Every APRS station says what it *is* in two characters, and a map draws an
icon from that pair. sdroxide ships those icons rather than fetching them, for
the same reason it ships the country flags: a map that only shows what stations
are once some third party's CDN answers is a map that looks different on every
machine, and the browser client has no business reaching past sdroxide for one.

They are drawn here rather than sourced from an existing icon set, so there is
no third-party artwork in the tree and no licence attached to it. This writes:

  * `crates/sdroxide-ui/assets/aprs/<kind>.png` — one 48x48 white-on-transparent
    image per `sdroxide_types::AprsSymbolKind`, drawn at 4x and downsampled, so
    the edges are properly antialiased. White so the panel can tint them: the
    map colours a station by how recently it was heard.
  * `crates/sdroxide-ui/src/aprs_icons.rs` — the `include_bytes!` table.

    ./tools/gen_aprs_icons.py
    ./tools/gen_aprs_icons.py --sheet /tmp/sheet.png   # a contact sheet to look at

The kind list has to match the enum in `crates/sdroxide-types/src/aprs.rs`; the
script reads it from there and fails if this file has drifted from it.

Coordinates are in a 0..1 box, x right and y down.
"""

import argparse
import io
import math
import pathlib
import re
import sys

from PIL import Image, ImageDraw

ROOT = pathlib.Path(__file__).resolve().parent.parent
ENUM = ROOT / "crates/sdroxide-types/src/aprs.rs"
OUT_DIR = ROOT / "crates/sdroxide-ui/assets/aprs"
OUT_RS = ROOT / "crates/sdroxide-ui/src/aprs_icons.rs"

SIZE = 48
SUPER = 4


# ── shape helpers ─────────────────────────────────────────────────────────

def circle_pts(cx, cy, r, n=48, a0=0.0, a1=2 * math.pi):
    return [(cx + r * math.cos(a0 + (a1 - a0) * i / n),
             cy + r * math.sin(a0 + (a1 - a0) * i / n)) for i in range(n + 1)]


def ellipse_pts(cx, cy, rx, ry, n=48):
    return [(cx + rx * math.cos(2 * math.pi * i / n),
             cy + ry * math.sin(2 * math.pi * i / n)) for i in range(n)]


def spiral_pts(cx, cy, r0, r1, turns, phase, n=40):
    out = []
    for i in range(n + 1):
        t = i / n
        a = phase + turns * 2 * math.pi * t
        r = r0 + (r1 - r0) * t
        out.append((cx + r * math.cos(a), cy + r * math.sin(a)))
    return out


def rot(pts, cx, cy, deg):
    a = math.radians(deg)
    ca, sa = math.cos(a), math.sin(a)
    return [(cx + (x - cx) * ca - (y - cy) * sa, cy + (x - cx) * sa + (y - cy) * ca)
            for x, y in pts]


def rect(x0, y0, x1, y1):
    return [(x0, y0), (x1, y0), (x1, y1), (x0, y1)]


def P(pts):
    """Filled polygon."""
    return ("poly", pts)


def L(pts, w):
    """Stroked polyline, round joins."""
    return ("line", pts, w)


def C(cx, cy, r):
    return ("circle", cx, cy, r)


def R(cx, cy, r, w):
    """Stroked circle."""
    return ("ring", cx, cy, r, w)


def cloud():
    """The cloud every weather icon sits under."""
    return [
        C(0.31, 0.36, 0.17),
        C(0.53, 0.29, 0.21),
        C(0.72, 0.39, 0.15),
        P(rect(0.31, 0.36, 0.72, 0.53)),
    ]


def wheels(*xs, y=0.775, r=0.10):
    return [C(x, y, r) for x in xs]


def star(cx, cy, r, points=5, inner=0.42, phase=-math.pi / 2):
    pts = []
    for i in range(points * 2):
        rr = r if i % 2 == 0 else r * inner
        a = phase + math.pi * i / points
        pts.append((cx + rr * math.cos(a), cy + rr * math.sin(a)))
    return P(pts)


def building():
    """A pitched roof over a stroked box — the base of every fixed-site icon."""
    return [P([(0.50, 0.08), (0.97, 0.42), (0.03, 0.42)]), L(rect(0.14, 0.42, 0.86, 0.93) + [(0.14, 0.42)], 0.09)]


# ── the icons ─────────────────────────────────────────────────────────────

ICONS = {
    # Generic shapes, for the table entries that have no picture of their own.
    "Unknown": [R(0.5, 0.5, 0.33, 0.09), C(0.5, 0.5, 0.10)],
    "Dot": [C(0.5, 0.5, 0.26)],
    "Circle": [R(0.5, 0.5, 0.33, 0.11)],
    "Box": [L(rect(0.16, 0.16, 0.84, 0.84) + [(0.16, 0.16)], 0.11)],
    # Four points, so it cannot be mistaken for the digipeater's five.
    "Star": [star(0.5, 0.5, 0.46, points=4, inner=0.34)],
    "Triangle": [L([(0.5, 0.10), (0.92, 0.86), (0.08, 0.86), (0.5, 0.10)], 0.10)],
    "X": [L([(0.16, 0.16), (0.84, 0.84)], 0.12), L([(0.84, 0.16), (0.16, 0.84)], 0.12)],

    # ── fixed sites ──
    "House": building(),
    "Hospital": [
        R(0.5, 0.5, 0.40, 0.09),
        P(rect(0.43, 0.24, 0.57, 0.76)),
        P(rect(0.24, 0.43, 0.76, 0.57)),
    ],
    # A graduation cap.
    "School": [
        P([(0.50, 0.16), (0.96, 0.37), (0.50, 0.58), (0.04, 0.37)]),
        L([(0.82, 0.43), (0.82, 0.74)], 0.07),
        C(0.82, 0.78, 0.08),
        P([(0.24, 0.46), (0.76, 0.46), (0.76, 0.62), (0.50, 0.72), (0.24, 0.62)]),
    ],
    "Restaurant": [
        L([(0.30, 0.10), (0.30, 0.34)], 0.06),
        L([(0.44, 0.10), (0.44, 0.34)], 0.06),
        L([(0.28, 0.34), (0.46, 0.34)], 0.06),
        L([(0.37, 0.34), (0.37, 0.92)], 0.08),
        P([(0.62, 0.08), (0.76, 0.16), (0.74, 0.50), (0.62, 0.50)]),
        L([(0.68, 0.50), (0.68, 0.92)], 0.08),
    ],
    "Church": [
        L([(0.50, 0.04), (0.50, 0.30)], 0.07),
        L([(0.37, 0.13), (0.63, 0.13)], 0.07),
        P([(0.50, 0.28), (0.82, 0.56), (0.18, 0.56)]),
        L(rect(0.22, 0.56, 0.78, 0.93) + [(0.22, 0.56)], 0.08),
    ],
    "Parking": [
        R(0.5, 0.5, 0.42, 0.09),
        L([(0.40, 0.76), (0.40, 0.26), (0.58, 0.26), (0.66, 0.35),
           (0.58, 0.47), (0.40, 0.47)], 0.10),
    ],
    # A tent, with the door left as the gap between the two halves.
    "Campground": [
        P([(0.47, 0.10), (0.47, 0.88), (0.06, 0.88)]),
        P([(0.53, 0.10), (0.94, 0.88), (0.59, 0.88)]),
    ],
    "Shelter": [
        P([(0.50, 0.02), (0.98, 0.32), (0.02, 0.32)]),
        C(0.50, 0.50, 0.13),
        # Wide shoulders: otherwise the head and the body merge into one stem
        # and the whole thing reads as a lamp.
        P([(0.24, 0.96), (0.76, 0.96), (0.70, 0.70), (0.30, 0.70)]),
    ],
    "Lighthouse": [
        P([(0.40, 0.20), (0.60, 0.20), (0.62, 0.36), (0.38, 0.36)]),
        P([(0.38, 0.36), (0.62, 0.36), (0.72, 0.93), (0.28, 0.93)]),
        L([(0.64, 0.24), (0.94, 0.14)], 0.07),
        L([(0.36, 0.24), (0.06, 0.14)], 0.07),
        L([(0.30, 0.62), (0.70, 0.62)], 0.06),
    ],
    "Firehouse": [
        P([(0.46, 0.00), (0.63, 0.11), (0.58, 0.18), (0.65, 0.24),
           (0.50, 0.36), (0.33, 0.26), (0.34, 0.14), (0.43, 0.10)]),
        P([(0.50, 0.30), (0.97, 0.56), (0.03, 0.56)]),
        L(rect(0.14, 0.56, 0.86, 0.94) + [(0.14, 0.56)], 0.08),
        P(rect(0.35, 0.68, 0.65, 0.94)),
    ],
    "PoliceStation": [
        P([(0.50, 0.06), (0.88, 0.21), (0.88, 0.54), (0.50, 0.94),
           (0.12, 0.54), (0.12, 0.21)]),
    ],

    # ── network infrastructure ──
    # Five points: the star every APRS operator already reads as a digipeater.
    "Digipeater": [star(0.5, 0.5, 0.48)],
    "Igate": [
        L([(0.32, 0.20), (0.50, 0.05), (0.68, 0.20)], 0.07),
        L([(0.50, 0.08), (0.50, 0.34)], 0.07),
        R(0.50, 0.63, 0.29, 0.08),
        L([(0.22, 0.63), (0.78, 0.63)], 0.07),
        L([(0.50, 0.34), (0.50, 0.92)], 0.06),
    ],
    "Node": [
        C(0.20, 0.24, 0.15), C(0.80, 0.24, 0.15), C(0.50, 0.80, 0.15),
        L([(0.20, 0.24), (0.80, 0.24)], 0.06),
        L([(0.20, 0.24), (0.50, 0.80)], 0.06),
        L([(0.80, 0.24), (0.50, 0.80)], 0.06),
    ],
    "Antenna": [
        L([(0.30, 0.94), (0.50, 0.30), (0.70, 0.94)], 0.09),
        L([(0.38, 0.68), (0.62, 0.68)], 0.07),
        # Arcs radiating off the tip. Two ticks read as a stick figure's arms.
        L(circle_pts(0.50, 0.28, 0.20, a0=math.radians(-72), a1=math.radians(-18)), 0.055),
        L(circle_pts(0.50, 0.28, 0.33, a0=math.radians(-70), a1=math.radians(-20)), 0.055),
        L(circle_pts(0.50, 0.28, 0.20, a0=math.radians(198), a1=math.radians(252)), 0.055),
        L(circle_pts(0.50, 0.28, 0.33, a0=math.radians(200), a1=math.radians(250)), 0.055),
    ],
    "Yagi": [
        L([(0.10, 0.50), (0.92, 0.50)], 0.07),
        L([(0.20, 0.20), (0.20, 0.80)], 0.07),
        L([(0.37, 0.26), (0.37, 0.74)], 0.07),
        L([(0.52, 0.31), (0.52, 0.69)], 0.07),
        L([(0.66, 0.35), (0.66, 0.65)], 0.07),
        L([(0.79, 0.39), (0.79, 0.61)], 0.07),
    ],
    "Dish": [
        P(rot(ellipse_pts(0.45, 0.38, 0.40, 0.15), 0.45, 0.38, -32)),
        L([(0.45, 0.38), (0.76, 0.16)], 0.05),
        C(0.78, 0.14, 0.08),
        L([(0.47, 0.44), (0.52, 0.90)], 0.08),
        L([(0.32, 0.93), (0.72, 0.93)], 0.08),
    ],
    "Server": [
        L(rect(0.10, 0.12, 0.90, 0.34) + [(0.10, 0.12)], 0.07),
        L(rect(0.10, 0.39, 0.90, 0.61) + [(0.10, 0.39)], 0.07),
        L(rect(0.10, 0.66, 0.90, 0.88) + [(0.10, 0.66)], 0.07),
        C(0.24, 0.23, 0.05), C(0.24, 0.50, 0.05), C(0.24, 0.77, 0.05),
    ],
    "Computer": [
        L(rect(0.08, 0.14, 0.92, 0.64) + [(0.08, 0.14)], 0.08),
        L([(0.50, 0.64), (0.50, 0.80)], 0.08),
        L([(0.26, 0.86), (0.74, 0.86)], 0.09),
    ],
    "Phone": [
        L(rect(0.30, 0.06, 0.70, 0.94) + [(0.30, 0.06)], 0.08),
        L([(0.42, 0.83), (0.58, 0.83)], 0.06),
        L([(0.36, 0.20), (0.64, 0.20)], 0.05),
    ],

    # ── land vehicles ──
    "Car": [
        P([(0.06, 0.72), (0.09, 0.55), (0.26, 0.55), (0.35, 0.36),
           (0.66, 0.36), (0.78, 0.55), (0.94, 0.57), (0.94, 0.72)]),
    ] + wheels(0.28, 0.74),
    "Truck": [
        P(rect(0.04, 0.28, 0.56, 0.70)),
        P([(0.58, 0.44), (0.76, 0.44), (0.92, 0.58), (0.92, 0.70), (0.58, 0.70)]),
    ] + wheels(0.22, 0.78),
    "Van": [
        P([(0.05, 0.32), (0.62, 0.32), (0.84, 0.52), (0.94, 0.52),
           (0.94, 0.72), (0.05, 0.72)]),
    ] + wheels(0.26, 0.76),
    "Bus": [
        L(rect(0.04, 0.24, 0.96, 0.70) + [(0.04, 0.24)], 0.08),
        L([(0.30, 0.28), (0.30, 0.48)], 0.06),
        L([(0.52, 0.28), (0.52, 0.48)], 0.06),
        L([(0.74, 0.28), (0.74, 0.48)], 0.06),
    ] + wheels(0.26, 0.76, r=0.09),
    "Motorcycle": [
        R(0.19, 0.68, 0.20, 0.11), R(0.81, 0.68, 0.20, 0.11),
        P([(0.22, 0.66), (0.32, 0.46), (0.54, 0.44), (0.60, 0.32),
           (0.72, 0.32), (0.72, 0.42), (0.63, 0.52), (0.78, 0.66)]),
        L([(0.58, 0.28), (0.80, 0.26)], 0.06),
    ],
    "Bicycle": [
        R(0.19, 0.68, 0.18, 0.05), R(0.81, 0.68, 0.18, 0.05),
        L([(0.19, 0.68), (0.42, 0.68), (0.57, 0.36), (0.81, 0.68)], 0.05),
        L([(0.42, 0.68), (0.57, 0.36)], 0.05),
        L([(0.30, 0.68), (0.50, 0.30)], 0.05),
        L([(0.44, 0.28), (0.62, 0.28)], 0.05),
    ],
    "Rv": [
        L(rect(0.03, 0.20, 0.66, 0.72) + [(0.03, 0.20)], 0.08),
        P(rect(0.10, 0.27, 0.33, 0.44)),
        L([(0.50, 0.44), (0.50, 0.72)], 0.06),
        P([(0.66, 0.44), (0.82, 0.44), (0.96, 0.56), (0.96, 0.72), (0.66, 0.72)]),
    ] + wheels(0.24, 0.80, r=0.09),
    "Train": [
        P(rect(0.08, 0.20, 0.44, 0.66)),
        P(rect(0.44, 0.38, 0.93, 0.66)),
        L([(0.02, 0.90), (0.98, 0.90)], 0.06),
    ] + wheels(0.22, 0.52, 0.80, y=0.76, r=0.09),
    "Ambulance": [
        L(rect(0.04, 0.30, 0.60, 0.70) + [(0.04, 0.30)], 0.07),
        P([(0.60, 0.44), (0.80, 0.44), (0.94, 0.57), (0.94, 0.70), (0.60, 0.70)]),
        P(rect(0.27, 0.37, 0.37, 0.63)),
        P(rect(0.19, 0.45, 0.45, 0.55)),
    ] + wheels(0.22, 0.78, r=0.09),
    "FireTruck": [
        L([(0.08, 0.34), (0.62, 0.16)], 0.06),
        L([(0.14, 0.24), (0.18, 0.36)], 0.04),
        L([(0.30, 0.20), (0.34, 0.32)], 0.04),
        L([(0.46, 0.16), (0.50, 0.28)], 0.04),
        P(rect(0.04, 0.42, 0.62, 0.70)),
        P([(0.62, 0.46), (0.80, 0.46), (0.95, 0.58), (0.95, 0.70), (0.62, 0.70)]),
    ] + wheels(0.22, 0.80, r=0.09),
    "Police": [
        P(rect(0.34, 0.16, 0.66, 0.28)),
        P([(0.06, 0.72), (0.09, 0.56), (0.26, 0.56), (0.35, 0.38),
           (0.66, 0.38), (0.78, 0.56), (0.94, 0.58), (0.94, 0.72)]),
    ] + wheels(0.28, 0.74, r=0.09),
    "Tractor": [
        P([(0.18, 0.44), (0.56, 0.44), (0.56, 0.32), (0.74, 0.32),
           (0.86, 0.54), (0.86, 0.68), (0.18, 0.68)]),
        R(0.30, 0.66, 0.24, 0.09),
        R(0.80, 0.76, 0.14, 0.08),
    ],

    # ── air and space ──
    "Aircraft": [
        P([(0.45, 0.04), (0.55, 0.04), (0.57, 0.60), (0.55, 0.94),
           (0.45, 0.94), (0.43, 0.60)]),
        P(rect(0.04, 0.44, 0.96, 0.56)),
        P(rect(0.28, 0.80, 0.72, 0.90)),
    ],
    "AircraftLarge": [
        P([(0.44, 0.02), (0.56, 0.02), (0.60, 0.68), (0.56, 0.96),
           (0.44, 0.96), (0.40, 0.68)]),
        P([(0.02, 0.74), (0.46, 0.34), (0.54, 0.34), (0.98, 0.74),
           (0.98, 0.83), (0.50, 0.56), (0.02, 0.83)]),
        P([(0.22, 0.94), (0.46, 0.76), (0.54, 0.76), (0.78, 0.94),
           (0.78, 0.99), (0.50, 0.86), (0.22, 0.99)]),
    ],
    "Helicopter": [
        L([(0.06, 0.24), (0.80, 0.24)], 0.06),
        L([(0.42, 0.24), (0.42, 0.44)], 0.06),
        P([(0.18, 0.46), (0.56, 0.42), (0.70, 0.54), (0.62, 0.70), (0.22, 0.70)]),
        P(rect(0.62, 0.54, 0.96, 0.62)),
        L([(0.92, 0.42), (0.92, 0.68)], 0.06),
        L([(0.14, 0.86), (0.70, 0.86)], 0.06),
        L([(0.26, 0.70), (0.24, 0.86)], 0.05),
        L([(0.56, 0.70), (0.58, 0.86)], 0.05),
    ],
    "Balloon": [
        C(0.50, 0.36, 0.29),
        L([(0.33, 0.58), (0.40, 0.74)], 0.05),
        L([(0.67, 0.58), (0.60, 0.74)], 0.05),
        P([(0.38, 0.74), (0.62, 0.74), (0.59, 0.93), (0.41, 0.93)]),
    ],
    "Glider": [
        P([(0.46, 0.04), (0.54, 0.04), (0.55, 0.96), (0.45, 0.96)]),
        P(rect(0.01, 0.40, 0.99, 0.46)),
        P(rect(0.32, 0.86, 0.68, 0.91)),
    ],
    # The body is an outline, not a fill: filled, its fins merge into it and
    # the whole thing reads as a dart.
    "Rocket": [
        L([(0.50, 0.04), (0.63, 0.30), (0.63, 0.72), (0.37, 0.72),
           (0.37, 0.30), (0.50, 0.04)], 0.08),
        P([(0.37, 0.52), (0.13, 0.84), (0.37, 0.76)]),
        P([(0.63, 0.52), (0.87, 0.84), (0.63, 0.76)]),
        P([(0.42, 0.80), (0.58, 0.80), (0.50, 0.99)]),
    ],
    "Satellite": [
        P(rect(0.39, 0.36, 0.61, 0.64)),
        P(rect(0.02, 0.40, 0.36, 0.60)),
        P(rect(0.64, 0.40, 0.98, 0.60)),
        L([(0.50, 0.36), (0.50, 0.16)], 0.06),
        L([(0.36, 0.10), (0.50, 0.20), (0.64, 0.10)], 0.06),
    ],
    "Boat": [
        P([(0.04, 0.64), (0.96, 0.64), (0.80, 0.88), (0.20, 0.88)]),
        P([(0.32, 0.40), (0.62, 0.40), (0.70, 0.62), (0.28, 0.62)]),
        L([(0.46, 0.40), (0.46, 0.18)], 0.05),
    ],
    "Yacht": [
        P([(0.06, 0.70), (0.94, 0.70), (0.78, 0.90), (0.22, 0.90)]),
        L([(0.50, 0.68), (0.50, 0.06)], 0.05),
        P([(0.54, 0.12), (0.88, 0.66), (0.54, 0.66)]),
        P([(0.46, 0.18), (0.46, 0.66), (0.14, 0.66)]),
    ],

    # ── people and events ──
    "Person": [
        C(0.50, 0.16, 0.13),
        P(rect(0.41, 0.30, 0.59, 0.62)),
        L([(0.16, 0.42), (0.84, 0.42)], 0.08),
        L([(0.50, 0.58), (0.30, 0.94)], 0.09),
        L([(0.50, 0.58), (0.70, 0.94)], 0.09),
    ],
    "Emergency": [
        L([(0.50, 0.06), (0.96, 0.90), (0.04, 0.90), (0.50, 0.06)], 0.09),
        P([(0.44, 0.36), (0.56, 0.36), (0.55, 0.66), (0.45, 0.66)]),
        C(0.50, 0.78, 0.06),
    ],
    "RedCross": [
        P([(0.38, 0.08), (0.62, 0.08), (0.62, 0.38), (0.92, 0.38),
           (0.92, 0.62), (0.62, 0.62), (0.62, 0.92), (0.38, 0.92),
           (0.38, 0.62), (0.08, 0.62), (0.08, 0.38), (0.38, 0.38)]),
    ],
    # A campfire rather than a bare flame. A flame silhouette on its own is a
    # teardrop at this size, whatever it is shaped like; the crossed logs are
    # what make it read as fire.
    "Fire": [
        P([(0.44, 0.04), (0.60, 0.22), (0.55, 0.32), (0.70, 0.44),
           (0.72, 0.60), (0.58, 0.74), (0.40, 0.74), (0.28, 0.60),
           (0.32, 0.44), (0.44, 0.36), (0.38, 0.22)]),
        L([(0.08, 0.90), (0.92, 0.80)], 0.09),
        L([(0.08, 0.80), (0.92, 0.90)], 0.09),
    ],
    "Eyeball": [
        L(ellipse_pts(0.50, 0.50, 0.46, 0.26) + [(0.96, 0.50)], 0.07),
        C(0.50, 0.50, 0.15),
    ],

    # ── weather ──
    "WxStation": [
        L([(0.50, 0.16), (0.50, 0.92)], 0.07),
        L([(0.28, 0.92), (0.72, 0.92)], 0.08),
        L([(0.16, 0.32), (0.84, 0.32)], 0.06),
        C(0.13, 0.32, 0.10), C(0.87, 0.32, 0.10), C(0.50, 0.13, 0.10),
    ],
    "Rain": cloud() + [
        L([(0.30, 0.64), (0.23, 0.86)], 0.07),
        L([(0.52, 0.64), (0.45, 0.86)], 0.07),
        L([(0.74, 0.64), (0.67, 0.86)], 0.07),
    ],
    "Snow": cloud() + [
        C(0.28, 0.72, 0.08), C(0.51, 0.85, 0.08), C(0.74, 0.70, 0.08),
    ],
    "Thunderstorm": cloud() + [
        P([(0.56, 0.56), (0.40, 0.80), (0.51, 0.80), (0.42, 0.99),
           (0.68, 0.72), (0.55, 0.72), (0.64, 0.56)]),
    ],
    "Hurricane": [
        C(0.50, 0.50, 0.11),
        L(spiral_pts(0.50, 0.50, 0.14, 0.46, 0.55, 0.0), 0.09),
        L(spiral_pts(0.50, 0.50, 0.14, 0.46, 0.55, math.pi), 0.09),
    ],
    "Tornado": [
        P([(0.06, 0.10), (0.94, 0.10), (0.60, 0.58), (0.58, 0.96),
           (0.42, 0.96), (0.42, 0.58)]),
        L([(0.16, 0.26), (0.80, 0.26)], 0.05),
        L([(0.28, 0.42), (0.70, 0.42)], 0.05),
    ],
    "Cloudy": cloud(),
    "Sunny": [C(0.50, 0.50, 0.24)] + [
        L([(0.50 + 0.34 * math.cos(a), 0.50 + 0.34 * math.sin(a)),
           (0.50 + 0.48 * math.cos(a), 0.50 + 0.48 * math.sin(a))], 0.07)
        for a in [math.pi * i / 4 for i in range(8)]
    ],
}


# ── rasteriser ────────────────────────────────────────────────────────────

def draw_icon(prims, size):
    """One icon as an alpha mask, drawn oversampled and downsampled."""
    n = size * SUPER
    img = Image.new("L", (n, n), 0)
    d = ImageDraw.Draw(img)
    s = lambda p: (p[0] * n, p[1] * n)
    for prim in prims:
        if prim[0] == "poly":
            d.polygon([s(p) for p in prim[1]], fill=255)
        elif prim[0] == "line":
            pts, w = prim[1], prim[2] * n
            d.line([s(p) for p in pts], fill=255, width=max(1, round(w)))
            # Round the joins and the caps by hand: PIL's `joint="curve"` only
            # rounds interior joins, and a bare cap leaves a square corner that
            # is very visible at this size.
            for p in pts:
                x, y = s(p)
                d.ellipse([x - w / 2, y - w / 2, x + w / 2, y + w / 2], fill=255)
        elif prim[0] == "circle":
            _, cx, cy, r = prim
            x, y, r = cx * n, cy * n, r * n
            d.ellipse([x - r, y - r, x + r, y + r], fill=255)
        elif prim[0] == "ring":
            _, cx, cy, r, w = prim
            x, y, r, w = cx * n, cy * n, r * n, w * n
            d.ellipse([x - r, y - r, x + r, y + r], outline=255, width=max(1, round(w)))
        else:
            raise ValueError(prim[0])
    img = img.resize((size, size), Image.LANCZOS)
    # White, with the drawing in the alpha channel: the panel tints these, so
    # the colour has to come from the caller rather than from the file.
    out = Image.new("RGBA", (size, size), (255, 255, 255, 0))
    out.putalpha(img)
    return out


def kinds_from_enum():
    """The variant list, read from the Rust enum so the two cannot drift."""
    src = ENUM.read_text()
    m = re.search(r"pub enum AprsSymbolKind \{(.*?)\n\}", src, re.S)
    if not m:
        sys.exit(f"could not find `enum AprsSymbolKind` in {ENUM}")
    return re.findall(r"^\s{4}([A-Z][A-Za-z]*),$", m.group(1), re.M)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--sheet", help="also write a contact sheet here, to look at")
    args = ap.parse_args()

    kinds = kinds_from_enum()
    missing = [k for k in kinds if k not in ICONS]
    extra = [k for k in ICONS if k not in kinds]
    if missing or extra:
        sys.exit(f"drifted from the enum — missing {missing}, extra {extra}")

    OUT_DIR.mkdir(parents=True, exist_ok=True)
    total = 0
    for kind in kinds:
        img = draw_icon(ICONS[kind], SIZE)
        buf = io.BytesIO()
        img.save(buf, "PNG", optimize=True)
        path = OUT_DIR / f"{kind}.png"
        path.write_bytes(buf.getvalue())
        total += len(buf.getvalue())

    rows = "\n".join(
        f'    (AprsSymbolKind::{k}, include_bytes!("../assets/aprs/{k}.png")),' for k in kinds
    )
    OUT_RS.write_text(f'''//! The packaged APRS station icons, and the texture cache that draws them.
//!
//! Generated by `tools/gen_aprs_icons.py` — the drawings live there, in a 0..1
//! coordinate box, and the table below is written from
//! [`AprsSymbolKind`]'s own variant list. Edit the script and re-run it rather
//! than editing this file.
//!
//! They are compiled in rather than fetched, for the reason the country flags
//! are: a map that only shows what stations *are* once some third party's CDN
//! answers is a map that looks different on every machine, and the browser
//! client has no business reaching past sdroxide for one. Drawn rather than
//! sourced, so there is no third-party artwork in the tree.
//!
//! White on transparent, so the caller supplies the colour — the map fades a
//! station's icon as it ages, and colours our own station differently.

use std::collections::HashMap;

use eframe::egui;
use sdroxide_types::AprsSymbolKind;

/// One 48x48 image per symbol kind, in the enum's own order.
static ICON_PNG: &[(AprsSymbolKind, &[u8])] = &[
{rows}
];

/// Lazily decoded icon textures, one per kind.
#[derive(Default)]
pub struct AprsIcons {{
    tex: HashMap<AprsSymbolKind, egui::TextureHandle>,
}}

impl AprsIcons {{
    /// The texture for `kind`, decoding it the first time it is asked for.
    ///
    /// `None` only if the PNG will not decode, which would be a build problem
    /// rather than a runtime one — the caller draws a plain dot instead, so a
    /// station is never invisible for want of a picture.
    pub fn get(
        &mut self,
        ctx: &egui::Context,
        kind: AprsSymbolKind,
    ) -> Option<&egui::TextureHandle> {{
        if !self.tex.contains_key(&kind) {{
            let bytes = ICON_PNG.iter().find(|(k, _)| *k == kind).map(|(_, b)| *b)?;
            let img = image::load_from_memory(bytes).ok()?.to_rgba8();
            let (w, h) = img.dimensions();
            let colour = egui::ColorImage::from_rgba_unmultiplied(
                [w as usize, h as usize],
                img.as_raw(),
            );
            let handle = ctx.load_texture(
                format!("aprs-icon-{{kind:?}}"),
                colour,
                egui::TextureOptions::LINEAR,
            );
            self.tex.insert(kind, handle);
        }}
        self.tex.get(&kind)
    }}

    /// Draw `kind` centred in `rect`, tinted `tint`.
    ///
    /// Falls back to a filled dot where the texture is missing: a station with
    /// no icon still has to be on the map.
    pub fn paint(
        &mut self,
        ui: &egui::Ui,
        rect: egui::Rect,
        kind: AprsSymbolKind,
        tint: egui::Color32,
    ) {{
        let uv = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
        match self.get(ui.ctx(), kind) {{
            Some(t) => {{
                ui.painter().image(t.id(), rect, uv, tint);
            }}
            None => {{
                ui.painter().circle_filled(rect.center(), rect.width() * 0.25, tint);
            }}
        }}
    }}
}}
''')
    print(f"{len(kinds)} icons, {total / 1024:.1f} KiB total -> {OUT_DIR}")

    if args.sheet:
        cols = 9
        rows_n = (len(kinds) + cols - 1) // cols
        cell, pad, label = 64, 8, 12
        sheet = Image.new("RGBA", (cols * (cell + pad) + pad,
                                   rows_n * (cell + pad + label) + pad), (18, 20, 28, 255))
        d = ImageDraw.Draw(sheet)
        for i, kind in enumerate(kinds):
            cx, cy = i % cols, i // cols
            x = pad + cx * (cell + pad)
            y = pad + cy * (cell + pad + label)
            icon = draw_icon(ICONS[kind], cell).convert("RGBA")
            # Tint the way the panel does, so the sheet shows what egui will.
            tinted = Image.new("RGBA", icon.size, (120, 235, 235, 255))
            tinted.putalpha(icon.getchannel("A"))
            sheet.alpha_composite(tinted, (x, y))
            d.text((x, y + cell + 1), kind[:11], fill=(190, 190, 200, 255))
        sheet.save(args.sheet)
        print(f"contact sheet -> {args.sheet}")


if __name__ == "__main__":
    main()
