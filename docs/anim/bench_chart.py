from manim import *

config.background_color = "#0d1117"
FG = "#c9d1d9"
MUTED = "#8b949e"
BLUE = "#58a6ff"
GREEN = "#3fb950"
RED = "#f85149"
PANEL = "#161b22"


class BenchmarkChart(Scene):
    def construct(self):
        title = Text("Tokens to locate files, per retrieval task", color=FG,
                     weight=BOLD, font_size=40).to_edge(UP, buff=0.55)
        sub = Text("blind ls / grep / find loops   vs   one indexed fastfind call",
                   color=MUTED, font_size=24).next_to(title, DOWN, buff=0.16)

        # --- hand-drawn bars (no LaTeX) ---
        base_y, max_h, scale = -1.7, 3.3, 3.3 / 120.0

        def bar(x, val, color, name, vlabel):
            h = val * scale
            rect = Rectangle(width=1.5, height=h, fill_color=color, fill_opacity=1.0, stroke_width=0)
            rect.move_to([x, base_y + h / 2, 0])
            vl = Text(vlabel, color=color, weight=BOLD, font_size=38).next_to(rect, UP, buff=0.14)
            nm = Text(name, color=FG, font_size=24).next_to(rect, DOWN, buff=0.18)
            return VGroup(rect, vl, nm)

        b0 = bar(-1.25, 110, RED, "blind agent", "110k")
        b1 = bar(1.25, 8.5, GREEN, "fastfind", "8.5k")
        axis = Line([-2.9, base_y, 0], [2.9, base_y, 0], color=MUTED, stroke_width=2)
        ytitle = Text("thousand tokens", color=MUTED, font_size=20).rotate(PI / 2) \
            .next_to(axis, LEFT, buff=0.1).shift(UP * 1.4)
        chart = VGroup(axis, ytitle, b0, b1)

        def chip(big, small, color):
            b = Text(big, color=color, weight=BOLD, font_size=46)
            s = Text(small, color=MUTED, font_size=20)
            inner = VGroup(b, s).arrange(DOWN, buff=0.14)
            box = RoundedRectangle(width=4.0, height=1.5, corner_radius=0.14,
                                   stroke_color=color, stroke_width=2.5,
                                   fill_color=PANEL, fill_opacity=1.0)
            inner.move_to(box.get_center())
            return VGroup(box, inner)

        chips = VGroup(
            chip("~30 ms", "search latency · 1.8M files", BLUE),
            chip("~14x", "fewer retrieval tokens", GREEN),
            chip("1 in 3", "blind file-opens wasted", RED),
        ).arrange(DOWN, buff=0.3)

        body = VGroup(chart, chips).arrange(RIGHT, buff=1.2, aligned_edge=UP)
        body.next_to(sub, DOWN, buff=0.4)
        self.add(title, sub, body)
