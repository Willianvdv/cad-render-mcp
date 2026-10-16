"""High-bypass turbofan, cut away (CadQuery). Built along -Z, then turned so the intake faces the
front view (-Y) with Z up. Every rotating stage is its own part, so it can spin.

usage: python3 examples/turbofan.py examples/turbofan.step   (needs CadQuery)
"""
import math
import sys

import cadquery as cq

CUT = cq.Solid.makeBox(4000, 4000, 8000, cq.Vector(-4000, 0, -6500))  # x<0, y>0 quadrant
TURN = ((0, 0, 0), (1, 0, 0), 90)  # build (x, y, z) -> world (x, -z, y)


def revolve(pts_outer, pts_inner):
    """Closed (r, z) profile: outer spline, then inner spline back to the start; revolved 360."""
    w = cq.Workplane("XZ").moveTo(*pts_outer[0]).spline(pts_outer[1:], includeCurrent=True)
    w = w.lineTo(*pts_inner[0]).spline(pts_inner[1:], includeCurrent=True).close()
    return w.revolve(360, (0, 0, 0), (0, 1, 0)).val()


def poly_revolve(pts):
    return cq.Workplane("XZ").polyline(pts).close().revolve(360, (0, 0, 0), (0, 1, 0)).val()


def cutaway(s):
    return s.cut(CUT)


def blade_row(z, r_in, r_out, n, chord, thick, stagger, disk=None):
    """n straight blades between r_in and r_out at axial position z, plus an optional disk."""
    solids = []
    base = cq.Solid.makeBox(r_out - r_in, thick, chord, cq.Vector(r_in, -thick / 2, z - chord / 2))
    base = base.rotate(cq.Vector(0, 0, z), cq.Vector(1, 0, z), stagger)
    for k in range(n):
        solids.append(base.rotate(cq.Vector(0, 0, 0), cq.Vector(0, 0, 1), 360.0 * k / n))
    if disk:
        solids.append(cq.Solid.makeCylinder(r_in + 4, disk, cq.Vector(0, 0, z - disk / 2)))
    return solids


def fan_blades(n=22):
    """Wide-chord swept fan blades: an elliptic section lofted root -> mid -> tip with twist.
    The tip section stays inside TIP_R everywhere: sqrt(1015^2 + (250 cos 35)^2) = 1035.5."""
    blade = (
        cq.Workplane("YZ").workplane(offset=320).center(0, -820)
        .ellipse(190, 24, rotation_angle=62)
        .workplane(offset=350).ellipse(240, 12, rotation_angle=47)
        .workplane(offset=345).ellipse(250, 5, rotation_angle=35)
        .loft(ruled=False).val()
    )
    return [blade.rotate(cq.Vector(0, 0, 0), cq.Vector(0, 0, 1), 360.0 * k / n) for k in range(n)]


def build():
    parts = {}
    parts["NACELLE"] = (cutaway(revolve(
        [(1000, 0), (1065, 22), (1125, -120), (1152, -700), (1120, -1600), (1012, -2400)],
        [(968, -2390), (1006, -1700), (1040, -1000), (1043, -620), (1012, -200), (982, -55), (1000, 0)],
    )), "#e9ebee")
    parts["SPINNER"] = (cq.Workplane("XZ").moveTo(0, -330)
                        .spline([(140, -420), (300, -620), (345, -720)], includeCurrent=True)
                        .lineTo(345, -760).lineTo(0, -760).close()
                        .revolve(360, (0, 0, 0), (0, 1, 0)).val(), "#c7ccd2")
    parts["FAN_BLADES"] = (cq.Compound.makeCompound(fan_blades()), "#1f242b")
    parts["FAN_DISK"] = (cq.Solid.makeCylinder(335, 160, cq.Vector(0, 0, -920)), "#59626b")
    parts["OGV"] = (cq.Compound.makeCompound(blade_row(-1330, 540, 1015, 44, 150, 7, 12)), "#7a838d")
    parts["CORE_CASE"] = (cutaway(poly_revolve([
        (515, -1040), (560, -1070), (535, -1600), (465, -2500), (480, -3000), (615, -3820), (600, -3830),
        (465, -3000), (450, -2500), (520, -1600), (500, -1090),
    ])), "#a3abb5")
    # LP booster: 3 rotors (on the fan shaft) and their stators
    lp, st = [], []
    for i in range(3):
        z = -1140 - i * 120
        lp += blade_row(z, 335, 492, 52, 55, 5, 30, disk=26)
        st += blade_row(z - 60, 340, 495, 60, 45, 4, -25)
    # HP compressor: 9 stages, the annulus narrowing towards the combustor
    hp = []
    for i in range(9):
        t = i / 8
        z = -1700 - i * 105
        r_in, r_out = 240 + 20 * t, 445 - 140 * t
        hp += blade_row(z, r_in, r_out, 64 + 4 * i, 48 - 12 * t, 4, 35, disk=24)
        st += blade_row(z - 52, r_in + 4, r_out, 70 + 4 * i, 38 - 10 * t, 3, -30)
    parts["LP_BOOSTER"] = (cq.Compound.makeCompound(lp), "#c3c9d0")
    parts["HP_COMPRESSOR"] = (cq.Compound.makeCompound(hp), "#d4d9df")
    parts["STATORS"] = (cq.Compound.makeCompound(st), "#7f8891")
    # combustor ring with fuel nozzles
    burners = [cq.Solid.makeTorus(340, 62, cq.Vector(0, 0, -2790), cq.Vector(0, 0, 1))]
    for k in range(18):
        a = 2 * math.pi * k / 18
        burners.append(cq.Solid.makeCylinder(
            14, 150, cq.Vector(470 * math.cos(a), 470 * math.sin(a), -2740), cq.Vector(-math.cos(a), -math.sin(a), 0)))
    parts["COMBUSTOR"] = (cq.Compound.makeCompound(burners), "#c46a2c")
    ht = []
    for i in range(2):
        ht += blade_row(-3010 - i * 120, 265, 405, 60, 60, 6, 40, disk=34)
    parts["HP_TURBINE"] = (cq.Compound.makeCompound(ht), "#9b6a45")
    lt = []
    for i in range(5):
        r_out = 430 + 32 * i
        lt += blade_row(-3290 - i * 105, 285, r_out, 72 + 6 * i, 55, 5, -38, disk=30)
    parts["LP_TURBINE"] = (cq.Compound.makeCompound(lt), "#7b5a43")
    parts["SHAFT"] = (cq.Solid.makeCylinder(85, 3450, cq.Vector(0, 0, -4100)), "#59626b")
    parts["EXHAUST_PLUG"] = (cq.Workplane("XZ").moveTo(0, -4520)
                             .spline([(150, -4300), (265, -3990), (290, -3830)], includeCurrent=True)
                             .lineTo(0, -3830).close().revolve(360, (0, 0, 0), (0, 1, 0)).val(), "#6f625a")
    parts["CORE_NOZZLE"] = (cutaway(poly_revolve([
        (615, -3820), (540, -4230), (526, -4230), (600, -3820)])), "#8d949c")
    return parts


asm = cq.Assembly(name="TURBOFAN")
for name, (shape, col) in build().items():
    rgb = [int(col[i:i + 2], 16) / 255 for i in (1, 3, 5)]
    asm.add(shape.rotate(cq.Vector(*TURN[0]), cq.Vector(*TURN[1]), TURN[2]), name=name, color=cq.Color(*rgb))
asm.export(sys.argv[1] if len(sys.argv) > 1 else "turbofan.step")
print("ok")
