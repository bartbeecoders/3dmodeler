#!/usr/bin/env python3
"""Build a 1:10 Eiffel Tower + Champ de Mars surroundings in the live modeler."""

from __future__ import annotations

import json
import math
import sys
import urllib.error
import urllib.request

URL = "http://127.0.0.1:8323"

# Tour Eiffel Brown and surroundings
IRON = [0.56, 0.38, 0.24]
IRON_DK = [0.38, 0.25, 0.16]
IRON_LT = [0.66, 0.47, 0.31]
STONE = [0.74, 0.70, 0.62]
STONE_DK = [0.58, 0.54, 0.47]
GOLD = [0.90, 0.72, 0.22]
GLASS = [0.45, 0.62, 0.72]
GRASS = [0.30, 0.46, 0.20]
GRASS_DK = [0.22, 0.36, 0.15]
PATH = [0.64, 0.60, 0.50]
PLAZA = [0.70, 0.67, 0.58]
WATER = [0.18, 0.38, 0.52]
CREAM = [0.86, 0.80, 0.70]
ZINC = [0.40, 0.44, 0.47]
WOOD = [0.46, 0.30, 0.17]
SOIL = [0.42, 0.34, 0.22]

TOWER_NAMES: list[str] = []
ERRORS = 0


def cmd(payload: dict, timeout: float = 20.0) -> dict:
    global ERRORS
    req = urllib.request.Request(
        URL,
        data=json.dumps(payload).encode(),
        headers={"Content-Type": "application/json"},
    )
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            data = json.loads(r.read().decode())
    except Exception as exc:  # noqa: BLE001
        ERRORS += 1
        print(f"ERR {payload.get('cmd')} {payload.get('new_name', '')}: {exc}", file=sys.stderr)
        return {"ok": False, "error": str(exc)}
    if data.get("ok") is False:
        ERRORS += 1
        print(f"FAIL {payload.get('cmd')} {payload.get('new_name', payload)}: {data}", file=sys.stderr)
    return data


def add(**kwargs) -> str | None:
    kwargs["cmd"] = "add_object"
    res = cmd(kwargs)
    name = res.get("name") or kwargs.get("new_name")
    return name


def tower(**kwargs) -> str | None:
    name = add(dynamic=False, bounciness=0.35, **kwargs)
    if name:
        TOWER_NAMES.append(name)
    return name


def look_align(dx: float, dy: float, dz: float) -> list[float]:
    """Euler XYZ degrees so local +Z points along (dx,dy,dz)."""
    length = math.sqrt(dx * dx + dy * dy + dz * dz)
    if length < 1e-9:
        return [0.0, 0.0, 0.0]
    dx, dy, dz = dx / length, dy / length, dz / length
    rx = math.degrees(math.atan2(-dy, math.hypot(dx, dz)))
    ry = math.degrees(math.atan2(dx, dz))
    return [rx, ry, 0.0]


def beam(name: str, p0, p1, radius: float, color, *, tower_part=True) -> str | None:
    dx, dy, dz = p1[0] - p0[0], p1[1] - p0[1], p1[2] - p0[2]
    length = math.sqrt(dx * dx + dy * dy + dz * dz)
    if length < 0.04:
        return None
    mid = [(p0[i] + p1[i]) * 0.5 for i in range(3)]
    fn = tower if tower_part else add
    return fn(
        primitive="cylinder",
        new_name=name,
        location=mid,
        rotation_euler_deg=look_align(dx, dy, dz),
        scale=[radius, radius, length / 2.0],
        color=color,
        smooth=True,
    )


def box(name: str, center, size, color, rot=None, *, tower_part=True, **extra) -> str | None:
    fn = tower if tower_part else add
    return fn(
        primitive="cube",
        new_name=name,
        location=list(center),
        rotation_euler_deg=list(rot or [0, 0, 0]),
        scale=[size[0] / 2.0, size[1] / 2.0, size[2] / 2.0],
        color=color,
        **extra,
    )


def lerp(a: float, b: float, t: float) -> float:
    return a + (b - a) * t


def hw(z: float) -> float:
    """Half-width of the tower's leg-center square at height z (1:10 meters)."""
    if z <= 5.70:
        t = max(0.0, z) / 5.70
        return 3.50 + 2.75 * (1.0 - t) ** 1.85
    if z <= 11.50:
        t = (z - 5.70) / 5.80
        return lerp(3.50, 2.00, t)
    if z <= 27.60:
        t = (z - 11.50) / 16.10
        return lerp(2.00, 0.80, t**1.12)
    if z <= 30.00:
        t = (z - 27.60) / 2.40
        return lerp(0.80, 0.13, t)
    t = min(1.0, (z - 30.00) / 3.00)
    return 0.13 * (1.0 - t)


def spread(z: float) -> float:
    """Half-size of one leg's chord square."""
    if z <= 5.70:
        return lerp(0.95, 0.68, z / 5.70)
    if z <= 11.50:
        return lerp(0.68, 0.42, (z - 5.70) / 5.80)
    if z <= 27.60:
        return lerp(0.42, 0.18, (z - 11.50) / 16.10)
    return lerp(0.18, 0.05, min(1.0, (z - 27.60) / 2.40))


def chord_xy(sx: int, sy: int, ox: int, oy: int, z: float) -> tuple[float, float]:
    s = spread(z)
    return sx * hw(z) + ox * s, sy * hw(z) + oy * s


def pt(sx: int, sy: int, ox: int, oy: int, z: float) -> tuple[float, float, float]:
    x, y = chord_xy(sx, sy, ox, oy, z)
    return (x, y, z)


LEGS = (("NE", 1, 1), ("SE", 1, -1), ("SW", -1, -1), ("NW", -1, 1))
CHORDS = (("oo", 1, 1), ("oi", 1, -1), ("io", -1, 1), ("ii", -1, -1))


def build_section(tag: str, zs: list[float], post_r: float, brace_r: float, faces: list[tuple[int, int, int, int]]):
    """Posts along zs, horizontals at intermediate stations, X-braces on given faces."""
    for lname, sx, sy in LEGS:
        for cname, ox, oy in CHORDS:
            for i in range(len(zs) - 1):
                beam(
                    f"{tag}_{lname}_{cname}_{i}",
                    pt(sx, sy, ox, oy, zs[i]),
                    pt(sx, sy, ox, oy, zs[i + 1]),
                    post_r,
                    IRON,
                )
        # horizontal rings at each station except the very ends shared with platforms
        for i, z in enumerate(zs):
            if i == 0 or i == len(zs) - 1:
                continue
            ring = [pt(sx, sy, ox, oy, z) for _, ox, oy in CHORDS]
            # CHORDS order oo, oi, io, ii — connect as a square
            pairs = [(0, 1), (0, 2), (1, 3), (2, 3)]
            for a, b in pairs:
                beam(f"{tag}H_{lname}_{i}_{a}{b}", ring[a], ring[b], brace_r * 0.9, IRON_DK)
        # X braces
        for i in range(len(zs) - 1):
            z0, z1 = zs[i], zs[i + 1]
            for fa, fb, fc, fd in faces:
                # face uses two chord indices
                p00 = pt(sx, sy, CHORDS[fa][1], CHORDS[fa][2], z0)
                p01 = pt(sx, sy, CHORDS[fb][1], CHORDS[fb][2], z0)
                p10 = pt(sx, sy, CHORDS[fa][1], CHORDS[fa][2], z1)
                p11 = pt(sx, sy, CHORDS[fb][1], CHORDS[fb][2], z1)
                beam(f"{tag}X_{lname}_{i}_{fa}{fb}a", p00, p11, brace_r, IRON_DK)
                beam(f"{tag}X_{lname}_{i}_{fa}{fb}b", p01, p10, brace_r, IRON_DK)


def arc_points(p0, p1, rise: float, n: int):
    """Elliptical arc from p0 to p1, peaking `rise` above the higher of the ends? no: rise above the chord."""
    pts = []
    mx = (p0[0] + p1[0]) * 0.5
    my = (p0[1] + p1[1]) * 0.5
    mz = (p0[2] + p1[2]) * 0.5
    for i in range(n + 1):
        t = i / n
        a = math.pi * t
        x = p0[0] + (p1[0] - p0[0]) * t
        y = p0[1] + (p1[1] - p0[1]) * t
        z = mz + rise * math.sin(a)
        # pull the mid slightly toward the tower center so the arch sits under P1
        pull = math.sin(a) * 0.35
        x += (0.0 - mx) * pull * 0.15
        y += (0.0 - my) * pull * 0.15
        pts.append((x, y, z))
    return pts


def build_arches():
    z0 = 1.15
    # springing points: inner-front of adjacent legs
    # North (+Y): NE io (-x relative) and NW oi
    springs = [
        ("N", pt(1, 1, -1, 1, z0), pt(-1, 1, 1, 1, z0), 4.15),
        ("S", pt(1, -1, -1, -1, z0), pt(-1, -1, 1, -1, z0), 4.15),
        ("E", pt(1, 1, 1, -1, z0), pt(1, -1, 1, 1, z0), 4.15),
        ("W", pt(-1, 1, -1, -1, z0), pt(-1, -1, -1, 1, z0), 4.15),
    ]
    for name, a, b, rise in springs:
        pts = arc_points(a, b, rise, 9)
        for i in range(len(pts) - 1):
            beam(f"Arch_{name}_{i}", pts[i], pts[i + 1], 0.13, IRON_LT)
        # inner decorative arch
        def inset(p, t=0.12):
            return (p[0] * (1 - t), p[1] * (1 - t), p[2] + 0.25)

        q0, q1 = inset(a), inset(b)
        ipts = arc_points(q0, q1, rise - 0.55, 7)
        for i in range(len(ipts) - 1):
            beam(f"ArchIn_{name}_{i}", ipts[i], ipts[i + 1], 0.07, IRON_DK)


def build_platforms():
    # P1 deck + truss belt
    box("P1_Deck", [0, 0, 5.78], [7.4, 7.4, 0.22], IRON_LT)
    box("P1_Truss", [0, 0, 5.52], [7.7, 7.7, 0.28], IRON_DK)
    # glass galleries along the four sides
    for i, (c, r) in enumerate(
        (
            ([0, 2.85, 6.35], [5.2, 1.15, 1.15]),
            ([0, -2.85, 6.35], [5.2, 1.15, 1.15]),
            ([2.85, 0, 6.35], [1.15, 5.2, 1.15]),
            ([-2.85, 0, 6.35], [1.15, 5.2, 1.15]),
        )
    ):
        box(f"P1_Gallery_{i}", c, r, GLASS)
    # railings
    for i, (c, s) in enumerate(
        (
            ([0, 3.62, 6.15], [7.4, 0.07, 0.55]),
            ([0, -3.62, 6.15], [7.4, 0.07, 0.55]),
            ([3.62, 0, 6.15], [0.07, 7.4, 0.55]),
            ([-3.62, 0, 6.15], [0.07, 7.4, 0.55]),
        )
    ):
        box(f"P1_Rail_{i}", c, s, IRON_DK)
    # corner lanterns
    for i, (sx, sy) in enumerate(((1, 1), (1, -1), (-1, 1), (-1, -1))):
        tower(
            primitive="sphere",
            new_name=f"P1_Lantern_{i}",
            location=[sx * 3.45, sy * 3.45, 6.85],
            scale=[0.16, 0.16, 0.16],
            color=GOLD,
            smooth=True,
        )
        add(
            primitive="light",
            new_name=f"P1_Light_{i}",
            location=[sx * 3.45, sy * 3.45, 6.95],
            color=[1.0, 0.82, 0.45],
            intensity=1.4,
        )

    # P2
    box("P2_Deck", [0, 0, 11.55], [4.3, 4.3, 0.18], IRON_LT)
    box("P2_Truss", [0, 0, 11.32], [4.5, 4.5, 0.22], IRON_DK)
    box("P2_Cabin", [0, 0, 12.15], [2.6, 2.6, 1.05], GLASS)
    for i, (c, s) in enumerate(
        (
            ([0, 2.12, 11.85], [4.3, 0.06, 0.45]),
            ([0, -2.12, 11.85], [4.3, 0.06, 0.45]),
            ([2.12, 0, 11.85], [0.06, 4.3, 0.45]),
            ([-2.12, 0, 11.85], [0.06, 4.3, 0.45]),
        )
    ):
        box(f"P2_Rail_{i}", c, s, IRON_DK)

    # P3 observation cabin
    box("P3_Deck", [0, 0, 27.62], [1.85, 1.85, 0.16], IRON_LT)
    box("P3_Cabin", [0, 0, 28.25], [1.55, 1.55, 1.15], GLASS)
    box("P3_Roof", [0, 0, 28.92], [1.7, 1.7, 0.18], IRON)
    for i, (c, s) in enumerate(
        (
            ([0, 0.90, 27.90], [1.85, 0.05, 0.40]),
            ([0, -0.90, 27.90], [1.85, 0.05, 0.40]),
            ([0.90, 0, 27.90], [0.05, 1.85, 0.40]),
            ([-0.90, 0, 27.90], [0.05, 1.85, 0.40]),
        )
    ):
        box(f"P3_Rail_{i}", c, s, IRON_DK)


def build_spire():
    # stacked tapering drums
    tower(
        primitive="cylinder",
        new_name="Spire_Base",
        location=[0, 0, 29.45],
        scale=[0.38, 0.38, 0.48],
        color=IRON,
        smooth=True,
    )
    tower(
        primitive="cylinder",
        new_name="Spire_Mid",
        location=[0, 0, 30.45],
        scale=[0.22, 0.22, 0.55],
        color=IRON,
        smooth=True,
    )
    tower(
        primitive="cylinder",
        new_name="Antenna",
        location=[0, 0, 31.70],
        scale=[0.06, 0.06, 0.85],
        color=IRON_LT,
        smooth=True,
    )
    tower(
        primitive="cone",
        new_name="Antenna_Tip",
        location=[0, 0, 32.72],
        scale=[0.07, 0.07, 0.22],
        color=GOLD,
        smooth=True,
    )
    tower(
        primitive="sphere",
        new_name="Beacon",
        location=[0, 0, 33.00],
        scale=[0.08, 0.08, 0.08],
        color=GOLD,
        smooth=True,
    )
    add(
        primitive="light",
        new_name="Beacon_Light",
        location=[0, 0, 33.15],
        color=[1.0, 0.85, 0.4],
        intensity=2.2,
    )


def build_piers():
    for lname, sx, sy in LEGS:
        cx, cy = sx * 6.15, sy * 6.15
        box(f"Pier_{lname}", [cx, cy, 0.35], [2.35, 2.35, 0.70], STONE)
        box(f"Footing_{lname}", [cx, cy, 0.08], [2.85, 2.85, 0.16], STONE_DK)


def build_cross_girders():
    """Girders linking adjacent legs just under P1 and P2."""
    for z, tag, r in ((5.45, "P1G", 0.09), (11.25, "P2G", 0.07)):
        pts = [(sx * hw(z), sy * hw(z), z) for _, sx, sy in LEGS]
        # NE-SE, SE-SW, SW-NW, NW-NE  (indices 0-1, 1-2, 2-3, 3-0)
        order = [0, 1, 2, 3, 0]
        for i in range(4):
            beam(f"{tag}_{i}", pts[order[i]], pts[order[i + 1]], r, IRON)


def build_center_shaft():
    # elevator suggestion from P2 to P3
    zs = [11.70, 16.5, 21.5, 26.4, 27.50]
    r = 0.22
    for i in range(4):
        ang = math.radians(45 + 90 * i)
        xs = [r * math.cos(ang) for _ in zs]
        ys = [r * math.sin(ang) for _ in zs]
        for j in range(len(zs) - 1):
            beam(
                f"Shaft_{i}_{j}",
                (xs[j], ys[j], zs[j]),
                (xs[j + 1], ys[j + 1], zs[j + 1]),
                0.045,
                IRON_DK,
            )


# ---------------------------------------------------------------------------
# Surroundings
# ---------------------------------------------------------------------------

def ground():
    add(
        primitive="cube",
        new_name="Lawn",
        location=[0, 8, -0.18],
        scale=[55, 62, 0.18],
        color=GRASS,
        dynamic=False,
        bounciness=0.12,
    )
    add(
        primitive="cube",
        new_name="Plaza",
        location=[0, 0, 0.03],
        scale=[11.5, 11.5, 0.06],
        color=PLAZA,
        dynamic=False,
        bounciness=0.2,
    )
    # Champ de Mars allées
    add(
        primitive="cube",
        new_name="Allee_N",
        location=[0, 28, 0.025],
        scale=[3.2, 18, 0.05],
        color=PATH,
        dynamic=False,
        bounciness=0.2,
    )
    add(
        primitive="cube",
        new_name="Allee_N2",
        location=[0, 48, 0.025],
        scale=[2.4, 8, 0.05],
        color=PATH,
        dynamic=False,
    )
    add(
        primitive="cube",
        new_name="Path_E",
        location=[16, 18, 0.025],
        scale=[1.6, 22, 0.05],
        color=PATH,
        dynamic=False,
    )
    add(
        primitive="cube",
        new_name="Path_W",
        location=[-16, 18, 0.025],
        scale=[1.6, 22, 0.05],
        color=PATH,
        dynamic=False,
    )
    add(
        primitive="cube",
        new_name="Quai",
        location=[0, -26, 0.04],
        scale=[40, 3.2, 0.08],
        color=[0.60, 0.58, 0.52],
        dynamic=False,
        bounciness=0.25,
    )
    add(
        primitive="cube",
        new_name="Seine",
        location=[0, -36, -0.12],
        scale=[48, 8.5, 0.18],
        color=WATER,
        dynamic=False,
        bounciness=0.05,
        smooth=True,
    )
    add(
        primitive="cube",
        new_name="Far_Bank",
        location=[0, -48, -0.05],
        scale=[48, 5, 0.2],
        color=GRASS_DK,
        dynamic=False,
    )


def building(name, origin, size, rot_z=0, floors=5):
    """Haussmann-ish block: cream body + zinc mansard + window dots."""
    ox, oy = origin
    w, d, h = size
    add(
        primitive="cube",
        new_name=f"{name}_Body",
        location=[ox, oy, h / 2],
        rotation_euler_deg=[0, 0, rot_z],
        scale=[w / 2, d / 2, h / 2],
        color=CREAM,
        dynamic=False,
        bounciness=0.2,
    )
    add(
        primitive="cube",
        new_name=f"{name}_Roof",
        location=[ox, oy, h + 1.15],
        rotation_euler_deg=[0, 0, rot_z],
        scale=[w / 2 + 0.15, d / 2 + 0.15, 1.2],
        color=ZINC,
        dynamic=False,
    )
    # cornice
    add(
        primitive="cube",
        new_name=f"{name}_Cornice",
        location=[ox, oy, h + 0.12],
        rotation_euler_deg=[0, 0, rot_z],
        scale=[w / 2 + 0.2, d / 2 + 0.2, 0.16],
        color=[0.80, 0.74, 0.64],
        dynamic=False,
    )
    # a few window panes on the south face
    for fi in range(floors):
        z = 1.4 + fi * 2.15
        for k in range(4):
            t = (k - 1.5) * (w * 0.18)
            add(
                primitive="cube",
                new_name=f"{name}_Win_{fi}_{k}",
                location=[ox + t, oy - d / 2 - 0.03, z],
                scale=[0.28, 0.04, 0.55],
                color=[0.28, 0.40, 0.50],
                dynamic=False,
            )


def lamp(name, x, y):
    add(
        primitive="cylinder",
        new_name=f"{name}_Pole",
        location=[x, y, 2.15],
        scale=[0.07, 0.07, 2.15],
        color=[0.18, 0.18, 0.2],
        smooth=True,
        dynamic=False,
    )
    add(
        primitive="sphere",
        new_name=f"{name}_Globe",
        location=[x, y, 4.45],
        scale=[0.22, 0.22, 0.22],
        color=[1.0, 0.92, 0.7],
        smooth=True,
        dynamic=False,
    )
    add(
        primitive="light",
        new_name=f"{name}_Light",
        location=[x, y, 4.5],
        color=[1.0, 0.88, 0.62],
        intensity=1.6,
    )


def bench(name, x, y, yaw, dynamic=True):
    add(
        primitive="cube",
        new_name=f"{name}_Seat",
        location=[x, y, 0.42],
        rotation_euler_deg=[0, 0, yaw],
        scale=[0.9, 0.28, 0.06],
        color=WOOD,
        dynamic=dynamic,
        density=450,
        bounciness=0.22,
    )
    add(
        primitive="cube",
        new_name=f"{name}_Back",
        location=[x, y, 0.72],
        rotation_euler_deg=[0, 0, yaw],
        scale=[0.9, 0.05, 0.28],
        color=WOOD,
        dynamic=dynamic,
        density=450,
        bounciness=0.22,
    )


def cafe():
    # tables + chairs (dynamic) under a static umbrella
    spots = [(-10.5, 11.0), (-8.2, 12.4), (-11.2, 13.5)]
    for i, (x, y) in enumerate(spots):
        add(
            primitive="cylinder",
            new_name=f"Table_{i}",
            location=[x, y, 0.72],
            scale=[0.45, 0.45, 0.05],
            color=[0.85, 0.82, 0.76],
            dynamic=True,
            density=600,
            bounciness=0.2,
        )
        add(
            primitive="cylinder",
            new_name=f"TableLeg_{i}",
            location=[x, y, 0.35],
            scale=[0.05, 0.05, 0.35],
            color=[0.25, 0.25, 0.26],
            dynamic=True,
            density=800,
        )
        add(
            primitive="cube",
            new_name=f"Chair_{i}",
            location=[x + 0.55, y, 0.45],
            scale=[0.22, 0.22, 0.42],
            color=[0.55, 0.22, 0.16],
            dynamic=True,
            density=350,
            bounciness=0.25,
        )
        add(
            primitive="cylinder",
            new_name=f"Cup_{i}",
            location=[x + 0.08, y + 0.05, 0.82],
            scale=[0.035, 0.035, 0.05],
            color=[0.92, 0.92, 0.9],
            dynamic=True,
            density=400,
            bounciness=0.15,
        )
    add(
        primitive="cylinder",
        new_name="Umbrella_Pole",
        location=[-10.0, 12.2, 1.2],
        scale=[0.04, 0.04, 1.2],
        color=[0.2, 0.2, 0.22],
        dynamic=False,
    )
    add(
        primitive="cone",
        new_name="Umbrella",
        location=[-10.0, 12.2, 2.55],
        rotation_euler_deg=[180, 0, 0],
        scale=[1.6, 1.6, 0.35],
        color=[0.15, 0.28, 0.55],
        dynamic=False,
        smooth=True,
    )


def cars():
    def car(name, x, y, yaw, color):
        body = add(
            primitive="cube",
            new_name=f"{name}_Body",
            location=[x, y, 0.55],
            rotation_euler_deg=[0, 0, yaw],
            scale=[2.15, 0.9, 0.42],
            color=color,
            dynamic=True,
            density=400,
            bounciness=0.18,
        )
        cabin = add(
            primitive="cube",
            new_name=f"{name}_Cabin",
            location=[x, y, 1.12],
            rotation_euler_deg=[0, 0, yaw],
            scale=[1.05, 0.82, 0.32],
            color=[0.55, 0.68, 0.78],
            dynamic=False,
        )
        if body and cabin:
            cmd({"cmd": "boolean_objects", "op": "union", "target": body, "tools": [cabin]})
            cmd({"cmd": "update_object", "object": body, "dynamic": True, "density": 400})

    car("Taxi", 8.5, -18.5, 12, [0.92, 0.78, 0.12])
    car("Citadine", 14.5, -17.8, -8, [0.12, 0.22, 0.48])
    car("Citadine2", -15.2, -19.2, 170, [0.62, 0.12, 0.12])


def fountain():
    add(
        primitive="cylinder",
        new_name="Fountain_Basin",
        location=[0, 16.5, 0.22],
        scale=[2.4, 2.4, 0.22],
        color=STONE,
        smooth=True,
        dynamic=False,
    )
    add(
        primitive="cylinder",
        new_name="Fountain_Water",
        location=[0, 16.5, 0.38],
        scale=[2.15, 2.15, 0.08],
        color=[0.35, 0.58, 0.72],
        smooth=True,
        dynamic=False,
    )
    add(
        primitive="cylinder",
        new_name="Fountain_Stem",
        location=[0, 16.5, 0.85],
        scale=[0.18, 0.18, 0.55],
        color=STONE_DK,
        smooth=True,
        dynamic=False,
    )
    add(
        primitive="torus",
        new_name="Fountain_Bowl",
        location=[0, 16.5, 1.35],
        scale=[0.7, 0.7, 0.7],
        color=STONE,
        smooth=True,
        dynamic=False,
    )
    add(
        primitive="sphere",
        new_name="Fountain_Jet",
        location=[0, 16.5, 1.85],
        scale=[0.18, 0.18, 0.28],
        color=[0.55, 0.75, 0.85],
        smooth=True,
        dynamic=False,
    )


def physics_props():
    # rubber balls
    for i, (x, y, z) in enumerate(
        ((4.2, 8.5, 0.28), (5.1, 9.2, 0.28), (3.6, 9.6, 0.28), (14.0, 6.0, 0.35))
    ):
        add(
            primitive="sphere",
            new_name=f"Ball_{i}",
            location=[x, y, z],
            scale=[0.18 if i < 3 else 0.28] * 3,
            color=[0.85, 0.12, 0.14] if i % 2 == 0 else [0.12, 0.28, 0.72],
            smooth=True,
            dynamic=True,
            density=250,
            bounciness=0.82,
        )
    # crates
    for i, (x, y) in enumerate(((6.8, 7.2), (7.5, 7.0), (7.15, 7.8))):
        add(
            primitive="cube",
            new_name=f"Crate_{i}",
            location=[x, y, 0.28 + (0.05 if i == 2 else 0)],
            rotation_euler_deg=[0, 0, 12 * i],
            scale=[0.28, 0.24, 0.26],
            color=[0.55, 0.38, 0.2],
            dynamic=True,
            density=380,
            bounciness=0.18,
        )
    # barrels
    for i, (x, y) in enumerate(((9.2, 5.4), (9.7, 5.1))):
        add(
            primitive="cylinder",
            new_name=f"Barrel_{i}",
            location=[x, y, 0.42],
            scale=[0.28, 0.28, 0.42],
            color=[0.40, 0.26, 0.14],
            smooth=True,
            dynamic=True,
            density=420,
            bounciness=0.2,
        )
    # traffic cones
    for i, (x, y) in enumerate(((7.8, -14.5), (8.6, -14.2), (9.3, -14.6))):
        add(
            primitive="cone",
            new_name=f"Cone_{i}",
            location=[x, y, 0.32],
            scale=[0.16, 0.16, 0.32],
            color=[0.92, 0.38, 0.08],
            smooth=True,
            dynamic=True,
            density=200,
            bounciness=0.15,
        )
    # picnic bottles / baguette-ish
    add(
        primitive="cylinder",
        new_name="Bottle_Wine",
        location=[-6.4, 9.2, 0.18],
        scale=[0.045, 0.045, 0.16],
        color=[0.15, 0.05, 0.08],
        smooth=True,
        dynamic=True,
        density=800,
        bounciness=0.1,
    )
    add(
        primitive="cylinder",
        new_name="Baguette",
        location=[-6.1, 9.5, 0.06],
        rotation_euler_deg=[0, 90, 25],
        scale=[0.04, 0.04, 0.32],
        color=[0.78, 0.58, 0.28],
        smooth=True,
        dynamic=True,
        density=250,
        bounciness=0.05,
    )
    add(
        primitive="cube",
        new_name="Picnic_Cloth_Board",
        location=[-6.3, 9.4, 0.02],
        scale=[0.7, 0.55, 0.015],
        color=[0.75, 0.18, 0.16],
        dynamic=True,
        density=150,
        bounciness=0.05,
    )
    # souvenir kiosk (later broken into bricks)
    add(
        primitive="cube",
        new_name="Kiosk",
        location=[11.5, 8.5, 1.15],
        scale=[1.1, 0.9, 1.15],
        color=[0.55, 0.22, 0.18],
        dynamic=False,
        bounciness=0.15,
    )
    add(
        primitive="cube",
        new_name="Kiosk_Roof",
        location=[11.5, 8.5, 2.45],
        scale=[1.35, 1.15, 0.12],
        color=[0.25, 0.22, 0.2],
        dynamic=False,
    )


def wrecking_ball():
    # hanging from P1, south-east underside — swings when play is hit
    add(
        primitive="sphere",
        new_name="WreckingBall",
        location=[4.6, -4.6, 1.55],
        scale=[0.55, 0.55, 0.55],
        color=[0.18, 0.18, 0.2],
        smooth=True,
        dynamic=True,
        density=2500,
        bounciness=0.35,
    )
    add(
        primitive="rope",
        new_name="WreckingRope",
        location=[3.4, -3.4, 5.4],
        length=4.6,
        radius=0.035,
        segments=14,
        color=IRON_DK,
        rope_start="P1_Truss",
        rope_start_point=[1.8, -1.8, -0.1],
        rope_end="WreckingBall",
        rope_end_point=[0, 0, 0.5],
    )


def flagpole():
    add(
        primitive="cylinder",
        new_name="FlagPole",
        location=[-14.0, 6.0, 4.0],
        scale=[0.06, 0.06, 4.0],
        color=[0.25, 0.25, 0.27],
        smooth=True,
        dynamic=False,
    )
    add(
        primitive="sphere",
        new_name="FlagFinial",
        location=[-14.0, 6.0, 8.08],
        scale=[0.09, 0.09, 0.09],
        color=GOLD,
        smooth=True,
        dynamic=False,
    )
    # French flag: three vertical cloths pinned to the pole
    colors = ([0.00, 0.20, 0.55], [0.95, 0.95, 0.95], [0.85, 0.12, 0.16])
    names = ("Flag_Blue", "Flag_White", "Flag_Red")
    # cloth is local XY, rotate 90° X so height hangs down (-Z)
    # width 0.85, height 1.7; center so hoist (u=0) sits on the pole
    for i, (col, name) in enumerate(zip(colors, names)):
        cx = -14.0 + 0.42 + i * 0.85
        add(
            primitive="cloth",
            new_name=name,
            location=[cx, 6.0, 7.15],
            rotation_euler_deg=[90, 0, 0],
            width=0.85,
            height=1.7,
            segments_u=6,
            segments_v=10,
            stiffness=0.18,
            color=list(col),
            cloth_anchors=[
                {"u": 0, "v": 0, "object": "FlagPole", "local_point": [0, 0, 3.15 - i * 0.02]},
                {"u": 0, "v": 5, "object": "FlagPole", "local_point": [0, 0, 2.30]},
                {"u": 0, "v": 10, "object": "FlagPole", "local_point": [0, 0, 1.45]},
            ]
            if i == 0
            else [
                {"u": 0, "v": 0, "object": names[i - 1], "local_point": [0.4, 0.85, 0]},
                {"u": 0, "v": 5, "object": names[i - 1], "local_point": [0.4, 0, 0]},
                {"u": 0, "v": 10, "object": names[i - 1], "local_point": [0.4, -0.85, 0]},
            ],
        )


def trees():
    positions = [
        (-18, 14),
        (-20, 22),
        (-12, 26),
        (18, 15),
        (21, 24),
        (12, 30),
        (-8, 34),
        (8, 36),
        (0, 42),
        (-22, 8),
        (22, 8),
        (-24, -8),
        (24, -6),
    ]
    for i, (x, y) in enumerate(positions):
        res = cmd(
            {
                "cmd": "place_library_object",
                "asset": "Realistic Tree",
                "location": [x, y, 0],
            }
        )
        names = [o.get("name") if isinstance(o, dict) else o for o in res.get("objects", [])]
        root = res.get("root") or (names[0] if names else None)
        if isinstance(root, dict):
            root = root.get("name")
        if root and i % 3 == 1:
            cmd({"cmd": "update_object", "object": root, "scale": [1.25, 1.25, 1.35]})
        elif root and i % 3 == 2:
            cmd({"cmd": "update_object", "object": root, "scale": [0.85, 0.85, 0.9]})


def lights_and_camera():
    # late-afternoon sun from the southwest
    sun_dir = look_align(0.48, 0.40, 0.78)
    add(
        primitive="sun",
        new_name="Sun",
        location=[48, 40, 70],
        rotation_euler_deg=sun_dir,
        color=[1.0, 0.92, 0.78],
        intensity=3.2,
        shadows=True,
    )
    add(
        primitive="light",
        new_name="Fill_Sky",
        location=[-20, -10, 40],
        color=[0.65, 0.75, 0.95],
        intensity=1.1,
    )
    # camera: Champ de Mars / Trocadéro-ish three-quarter
    eye = [26.0, -24.0, 7.5]
    target = [0.0, 2.0, 11.0]
    back = [eye[0] - target[0], eye[1] - target[1], eye[2] - target[2]]
    add(
        primitive="camera",
        new_name="HeroCam",
        location=eye,
        rotation_euler_deg=look_align(*back),
        fov_deg=38,
        clip_start=0.2,
        clip_end=250,
    )
    # second camera, low plaza
    eye2 = [14.0, -16.0, 2.2]
    target2 = [0.0, 0.0, 8.0]
    back2 = [eye2[0] - target2[0], eye2[1] - target2[1], eye2[2] - target2[2]]
    add(
        primitive="camera",
        new_name="PlazaCam",
        location=eye2,
        rotation_euler_deg=look_align(*back2),
        fov_deg=42,
        clip_start=0.15,
        clip_end=200,
    )


def people():
    spots = [(-5.5, 10.2, 200), (5.8, 9.0, -30), (2.2, 14.8, 10), (-3.0, -8.5, 160)]
    colors = (
        [0.18, 0.22, 0.45],
        [0.45, 0.18, 0.16],
        [0.15, 0.35, 0.22],
        [0.25, 0.25, 0.28],
    )
    for i, ((x, y, yaw), col) in enumerate(zip(spots, colors)):
        add(
            primitive="cylinder",
            new_name=f"Person_{i}_Body",
            location=[x, y, 0.85],
            scale=[0.18, 0.14, 0.55],
            color=list(col),
            smooth=True,
            dynamic=False,
        )
        add(
            primitive="sphere",
            new_name=f"Person_{i}_Head",
            location=[x, y, 1.55],
            scale=[0.12, 0.12, 0.14],
            color=[0.78, 0.60, 0.46],
            smooth=True,
            dynamic=False,
        )


def finish():
    root = add(
        primitive="empty",
        new_name="Eiffel Tower",
        location=[0, 0, 0],
        group=True,
        show_label=True,
    )
    if root and len(TOWER_NAMES) >= 2:
        # group in chunks to avoid a huge payload
        chunk = 80
        first = True
        for i in range(0, len(TOWER_NAMES), chunk):
            part = TOWER_NAMES[i : i + chunk]
            if first:
                cmd({"cmd": "group_objects", "objects": [root] + part, "root": root})
                first = False
            else:
                for child in part:
                    cmd({"cmd": "set_parent", "child": child, "parent": root})
    cmd({"cmd": "add_measurement", "a": [0, 0, 0], "b": [0, 0, 33.0]})
    cmd({"cmd": "set_view", "shading": "shaded", "lighting": "scene"})


def main():
    print("reset scene…")
    cmd({"cmd": "new_scene"})
    cmd({"cmd": "delete_object", "object": "Cube"})

    print("ground…")
    ground()

    print("tower piers + legs…")
    build_piers()
    # outer faces of each leg: oo-oi and oo-io
    outer_faces = [(0, 1), (0, 2)]
    faces = [(a, b, 0, 0) for a, b in outer_faces]
    # rebuild faces as 4-tuples expected by build_section — it only uses first two
    face_ids = [(0, 1, 0, 0), (0, 2, 0, 0)]

    build_section("L1", [0.75, 2.45, 4.10, 5.55], 0.095, 0.038, face_ids)
    build_section("L2", [5.85, 8.65, 11.35], 0.075, 0.032, face_ids)
    build_section("L3", [11.65, 16.8, 22.0, 27.45], 0.055, 0.026, face_ids)

    print("arches + platforms…")
    build_arches()
    build_cross_girders()
    build_platforms()
    build_center_shaft()
    build_spire()

    print("surroundings…")
    building("Haussmann_E", (28, -8), (12, 8, 11.5), floors=5)
    building("Haussmann_W", (-30, -6), (11, 7.5, 10.5), floors=4)
    building("Haussmann_NE", (32, 18), (10, 9, 12.5), floors=5)
    fountain()
    for i, (x, y) in enumerate(
        ((-8, 8), (8, 8), (-7, 20), (7, 20), (-14, -12), (14, -12), (0, 32))
    ):
        lamp(f"Lamp_{i}", x, y)
    bench("Bench_A", -7.2, 10.5, 90, dynamic=True)
    bench("Bench_B", 7.2, 10.8, -90, dynamic=True)
    bench("Bench_C", -4.5, 22.0, 0, dynamic=True)
    cafe()
    cars()
    people()
    physics_props()
    flagpole()
    wrecking_ball()

    print("trees…")
    trees()

    print("lights / camera / group…")
    lights_and_camera()
    finish()

    print(f"done. tower parts={len(TOWER_NAMES)} errors={ERRORS}")


if __name__ == "__main__":
    main()
