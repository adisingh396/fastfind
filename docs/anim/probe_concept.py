from manim import *

config.background_color = "#0d1117"
FG = "#c9d1d9"
MUTED = "#8b949e"
BLUE = "#58a6ff"
GREEN = "#3fb950"
RED = "#f85149"
PANEL = "#161b22"


def _agent(pos):
    c = Circle(radius=0.5, color=BLUE, fill_color=PANEL, fill_opacity=1, stroke_width=3).move_to(pos)
    t = Text("AI", color=FG, font_size=22).move_to(pos)
    return VGroup(c, t)


class ProbeConcept(Scene):
    def construct(self):
        title = Text("One grounded call replaces the probing loop", color=FG,
                     weight=BOLD, font_size=40).to_edge(UP, buff=0.35)

        div = Line([0, -3.4, 0], [0, 2.0, 0], color="#30363d", stroke_width=2)
        vs = Text("vs", color=MUTED, font_size=28).move_to([0, -0.4, 0])

        def folder(pos):
            return RoundedRectangle(width=1.0, height=0.72, corner_radius=0.07, stroke_color=MUTED,
                                    stroke_width=2, fill_color=PANEL, fill_opacity=1).move_to(pos)

        # ---- left: blind ----
        lhead = Text("BLIND AGENT", color=RED, weight=BOLD, font_size=26).move_to([-3.6, 2.6, 0])
        la = _agent([-6.0, 0.1, 0])
        left = VGroup(lhead, la)
        for p in [[-4.6, 1.4, 0], [-2.4, 0.9, 0], [-4.6, -1.1, 0], [-2.3, -1.7, 0]]:
            f = folder(p)
            ar = Arrow(la.get_right(), [p[0] - 0.55, p[1], 0], buff=0.12, color=RED, stroke_width=3,
                       max_tip_length_to_length_ratio=0.1)
            x = Text("✗", color=RED, font_size=26).move_to([p[0] + 0.55, p[1] + 0.45, 0])
            left.add(ar, f, x)
        left.add(Text("ls · find · grep · cat …", color=RED, font_size=22, font="monospace").move_to([-3.6, -2.55, 0]))
        left.add(Text("50–200 loops · ~110k tokens", color=MUTED, font_size=22).move_to([-3.6, -3.15, 0]))

        # ---- right: fastfind ----
        rhead = Text("fastfind · MCP", color=GREEN, weight=BOLD, font_size=26).move_to([3.6, 2.6, 0])
        ra = _agent([1.0, 0.1, 0])
        fbox = RoundedRectangle(width=3.2, height=0.9, corner_radius=0.1, stroke_color=GREEN,
                                stroke_width=3, fill_color=PANEL, fill_opacity=1).move_to([4.9, 0.1, 0])
        fpath = Text("report.pdf", color=FG, font_size=22, font="monospace").move_to(fbox)
        rar = Arrow(ra.get_right(), fbox.get_left(), buff=0.12, color=GREEN, stroke_width=5)
        call = Text('search(ext="pdf")', color=BLUE, font_size=20, font="monospace").move_to([2.75, 1.0, 0])
        chk = Text("✓", color=GREEN, font_size=40).move_to([5.9, 1.0, 0])
        rcap = Text("1 call · ~30 ms · exact path", color=MUTED, font_size=22).move_to([3.6, -3.15, 0])
        right = VGroup(rhead, ra, fbox, fpath, rar, call, chk, rcap)

        self.add(title, div, vs, left, right)


class DemoPlaceholder(Scene):
    def construct(self):
        tri = Triangle(color=GREEN, fill_color=GREEN, fill_opacity=1).scale(0.55).rotate(-PI / 2)
        ring = Circle(radius=1.1, color=GREEN, stroke_width=4)
        play = VGroup(ring, tri).move_to([0, 0.9, 0])
        t1 = Text("live demo", color=FG, weight=BOLD, font_size=44).next_to(play, DOWN, buff=0.5)
        t2 = Text('"find every file that imports numpy" — one grep call across the disk',
                  color=MUTED, font_size=24).next_to(t1, DOWN, buff=0.25)
        t3 = Text("drop your screen recording here (docs/demo.gif)", color="#484f58",
                  font_size=18).next_to(t2, DOWN, buff=0.35)
        self.add(play, t1, t2, t3)
