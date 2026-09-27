from manim import *

config.background_color = "#0d1117"
FG = "#c9d1d9"
MUTED = "#8b949e"
BLUE = "#58a6ff"
GREEN = "#3fb950"


class Banner(Scene):
    def construct(self):
        title = Text("fastfind", color=FG, weight=BOLD, font_size=120)
        bar = Line(LEFT * 1.1, RIGHT * 1.1, color=GREEN, stroke_width=7).next_to(title, DOWN, buff=0.3)
        tag = Text("grounded file discovery for AI agents", color=BLUE, font_size=42) \
            .next_to(bar, DOWN, buff=0.35)
        sub = Text("one MCP call  ·  exact paths  ·  milliseconds", color=MUTED, font_size=28) \
            .next_to(tag, DOWN, buff=0.25)
        VGroup(title, bar, tag, sub).move_to(ORIGIN)
        self.add(title, bar, tag, sub)
