from manim import *

config.background_color = "#0d1117"
FG = "#c9d1d9"
MUTED = "#8b949e"
BLUE = "#58a6ff"
GREEN = "#3fb950"
RED = "#f85149"
PANEL = "#161b22"


class ProbeConcept(Scene):
    def construct(self):
        title = Text("One grounded call replaces the probing loop", color=FG,
                     weight=BOLD, font_size=38).to_edge(UP, buff=0.5)

        # divider
        div = Line([0, -3.2, 0], [0, 2.6, 0], color="#30363d", stroke_width=2)
        vs = Text("vs", color=MUTED, font_size=28).move_to([0, -0.2, 0])

        def agent(pos):
            c = Circle(radius=0.5, color=BLUE, fill_color=PANEL, fill_opacity=1, stroke_width=3).move_to(pos)
            t = Text("AI", color=FG, font_size=22).move_to(pos)
            return VGroup(c, t)

        def folder(pos):
            return RoundedRectangle(width=1.0, height=0.72, corner_radius=0.07,
                                    stroke_color=MUTED, stroke_width=2,
                                    fill_color=PANEL, fill_opacity=1).move_to(pos)

        # ---- left: blind ----
        lhead = Text("BLIND AGENT", color=RED, weight=BOLD, font_size=26).move_to([-3.6, 2.9, 0])
        la = agent([-6.1, 0.3, 0])
        spots = [[-4.6, 2.0, 0], [-2.3, 1.4, 0], [-4.7, -0.7, 0], [-2.4, -1.3, 0]]
        left = VGroup(lhead, la)
        for p in spots:
            f = folder(p)
            ar = Arrow(la.get_right(), [p[0] - 0.55, p[1], 0], buff=0.1, color=RED, stroke_width=3,
                       max_tip_length_to_length_ratio=0.12)
            x = Text("✗", color=RED, font_size=26).move_to([p[0] + 0.55, p[1] + 0.4, 0])
            left.add(ar, f, x)
        lcmd = Text("ls · find · grep · cat …", color=RED, font_size=22,
                    font="monospace").move_to([-3.6, -2.2, 0])
        lcap = Text("50–200 loops · ~110k tokens", color=MUTED, font_size=22).move_to([-3.6, -2.8, 0])
        left.add(lcmd, lcap)

        # ---- right: fastfind ----
        rhead = Text("fastfind · MCP", color=GREEN, weight=BOLD, font_size=26).move_to([3.6, 2.9, 0])
        ra = agent([1.5, 0.3, 0])
        fbox = RoundedRectangle(width=3.4, height=0.9, corner_radius=0.1, stroke_color=GREEN,
                                stroke_width=3, fill_color=PANEL, fill_opacity=1).move_to([5.0, 0.3, 0])
        fpath = Text("report.pdf", color=FG, font_size=22, font="monospace").move_to(fbox)
        rar = Arrow(ra.get_right(), fbox.get_left(), buff=0.1, color=GREEN, stroke_width=5)
        rcall = Text('search(ext="pdf")', color=BLUE, font_size=20, font="monospace").next_to(rar, UP, buff=0.15)
        chk = Text("✓", color=GREEN, font_size=40).next_to(fbox, UP, buff=0.1)
        rcap = Text("1 call · ~30 ms · exact path", color=MUTED, font_size=22).move_to([3.6, -2.8, 0])
        right = VGroup(rhead, ra, fbox, fpath, rar, rcall, chk, rcap)

        self.add(title, div, vs, left, right)
