#!/usr/bin/env python3
"""Execute the scalar body WGSL expressions for representative fragment inputs.

This is a narrow software evaluator, not a GPU/driver rendering test. Each RGB
channel is evaluated independently; the WGSL body uses component-wise math.
"""
import math
from pathlib import Path
import re
import unittest
from types import SimpleNamespace

SHADER = Path(__file__).resolve().parents[1] / 'src/shader.wgsl'


def shade(distance, fade, track=0.4, border=0.9, radius=100.0):
    source = SHADER.read_text().split('fn body(i: VsOut) -> FsOut {', 1)[1].split('\n}', 1)[0]
    env = {'i': SimpleNamespace(half=SimpleNamespace(x=radius, y=radius),
                                local=SimpleNamespace(x=0.0, y=distance),
                                color=SimpleNamespace(rgb=track, a=fade), border=border),
           'BODY_SHADOW': 0.078125, 'BODY_BORDER': 0.1875,
           'max': max, 'min': min, 'abs': abs,
           'clamp': lambda x, lo, hi: max(lo, min(hi, x)),
           'mix': lambda a, b, t: a * (1.0 - t) + b * t,
           'length': lambda v: math.hypot(*v),
           'vec2': lambda x, y: (x, y), 'vec3': lambda x: x,
           'vec4': lambda rgb, alpha: (rgb, alpha)}
    for line in source.splitlines():
        line = line.split('//')[0].strip()
        match = re.match(r'(?:let|var) (\w+) = (.*);$', line)
        if match:
            name, expr = match.groups()
        elif line.startswith('o.color = vec4'):
            name, expr = 'result', line[len('o.color = '):-1]
        else:
            continue
        expr = re.sub(r'vec([234])<f32>', r'vec\1', expr)
        env[name] = eval(expr, {'__builtins__': {}}, env)
        if name == 'result':
            return env[name]
    raise AssertionError('body shader output not found')


class BodyShaderMath(unittest.TestCase):
    def assert_close(self, actual, expected):
        self.assertAlmostEqual(actual, expected, places=6)

    def test_track_border_transition_coverage_and_fade(self):
        # At r=100, the inner border edge is 81.25 and the visible outer
        # edge 92.1875. The border transition spans one pixel.
        for distance, opacity in [(0.0, 0.7), (80.75, 0.7),
                                  (81.25, 0.85), (81.75, 1.0), (90.0, 1.0)]:
            for fade in (1.0, 0.4):
                rgb, alpha = shade(distance, fade)
                self.assert_close(alpha, opacity * fade)
                # Evaluate with black track / white border to check that RGB
                # is also attenuated by the same premultiplied alpha.
                c, a = shade(distance, fade, track=0.0, border=1.0)
                full_c, full_a = shade(distance, 1.0, track=0.0, border=1.0)
                self.assert_close(c, full_c * fade)
                self.assert_close(a, full_a * fade)
                self.assertLessEqual(rgb, alpha + 1e-7)
        # Premultiplied interpolation, not a straight-colour interpolation
        # multiplied by the interpolated opacity.
        inside_rgb, _ = shade(81.25, 1.0, track=0.0, border=0.0)
        middle_rgb, _ = shade(81.25, 1.0, track=0.0, border=1.0)
        self.assert_close(middle_rgb - inside_rgb, 0.5)
        edge_rgb, edge_alpha = shade(92.1875, 0.4)
        full_rgb, _ = shade(90.0, 0.4)
        self.assert_close(edge_alpha, 0.2)
        self.assert_close(edge_rgb, full_rgb * 0.5)
        self.assert_close(shade(94.0, 1.0)[1], 0.0)
        self.assertEqual(shade(0.0, 0.0), (0.0, 0.0))


if __name__ == '__main__':
    unittest.main()
