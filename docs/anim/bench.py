from manim import *

config.background_color = "#0d1117"

FG = "#c9d1d9"
MUTED = "#8b949e"
BLUE = "#58a6ff"
GREEN = "#3fb950"
RED = "#f85149"
PANEL = "#161b22"


class BlindAgentTax(Scene):
    """Static benchmark comparison (render last frame as PNG)."""

    def construct(self):
        title = Text("The blind-agent tax", color=FG, weight=BOLD, font_size=52)
        sub = Text("finding one file, two ways", color=MUTED, font_size=26)
        header = VGroup(title, sub).arrange(DOWN, buff=0.18).to_edge(UP, buff=0.5)
        self.play(Write(title), FadeIn(sub, shift=UP * 0.2))

        def panel(tag, tag_color, lines, accent):
            box = RoundedRectangle(width=6.1, height=4.3, corner_radius=0.15,
                                   stroke_color=accent, stroke_width=3,
                                   fill_color=PANEL, fill_opacity=1.0)
            head = Text(tag, color=tag_color, weight=BOLD, font_size=27)
            body = VGroup(*[Text(t, color=FG, font_size=23) for t in lines]) \
                .arrange(DOWN, aligned_edge=LEFT, buff=0.34)
            content = VGroup(head, body).arrange(DOWN, aligned_edge=LEFT, buff=0.4)
            content.move_to(box.get_center())
            return VGroup(box, content)

        blind = panel(
            "BLIND AGENT   ls · grep · find", RED,
            ["≈110k tokens per retrieval task",
             "50–200 probe loops / session",
             "65% file precision",
             "1 in 3 opened files wasted",
             "$2–5 per complex query"],
            RED,
        )
        fast = panel(
            "fastfind · MCP", GREEN,
            ["1  search()  call",
             "≈30 ms · exact full paths",
             "returns size + mtime + dirs",
             "grounded — 0 wasted reads",
             "~14× fewer retrieval tokens"],
            GREEN,
        )
        row = VGroup(blind, fast).arrange(RIGHT, buff=0.9).next_to(header, DOWN, buff=0.55)

        self.play(FadeIn(blind, shift=RIGHT * 0.3))
        self.play(FadeIn(fast, shift=LEFT * 0.3))
        arrow = Arrow(blind.get_right(), fast.get_left(), buff=0.1, color=BLUE, stroke_width=6)
        self.play(GrowArrow(arrow))

        foot = Text("reported by ContextBench · Semble · Hypergrep analyses (2025–26)",
                    color=MUTED, font_size=17).to_edge(DOWN, buff=0.3)
        self.play(FadeIn(foot))
        self.wait(0.5)


class ProbeVsFind(Scene):
    """Animated loop (render as gif): blind probing vs one grounded call."""

    def construct(self):
        cap = Text("blind agent: dozens of ls / grep / find loops",
                   color=MUTED, font_size=26).to_edge(UP, buff=0.5)
        self.play(FadeIn(cap))

        agent = VGroup(
            Circle(radius=0.5, color=BLUE, fill_color=PANEL, fill_opacity=1, stroke_width=3),
            Text("agent", color=FG, font_size=20),
        )
        agent.to_edge(LEFT, buff=1.2)
        self.play(FadeIn(agent))

        # scattered folders
        import random
        random.seed(7)
        folders = VGroup()
        spots = [(2.2, 2.0), (4.6, 1.1), (3.4, -0.4), (5.2, -1.7), (2.0, -2.1), (4.0, 2.4)]
        for (x, y) in spots:
            f = RoundedRectangle(width=1.1, height=0.8, corner_radius=0.08,
                                 stroke_color=MUTED, stroke_width=2,
                                 fill_color=PANEL, fill_opacity=1).move_to([x, y, 0])
            folders.add(f)
        self.play(LaggedStart(*[Create(f) for f in folders], lag_ratio=0.1, run_time=1.0))

        col = [RED]
        vt = ValueTracker(0)
        counter = always_redraw(
            lambda: Text(f"{int(vt.get_value()):,}", color=col[0], font_size=40).to_corner(UR, buff=0.8)
        )
        clabel = Text("tokens burned", color=MUTED, font_size=18).next_to(counter, DOWN, buff=0.1)
        self.add(counter)
        self.play(FadeIn(clabel))

        cmds = ["ls", "find", "grep", "cat", "ls -R", "grep -r", "find ."]
        vals = [3200, 8100, 12800, 15400, 18200, 20100, 21536]
        for i, (x, y) in enumerate([spots[k % len(spots)] for k in range(7)]):
            cmd = Text(cmds[i], color=RED, font_size=20, font="monospace")
            arr = Arrow(agent.get_right(), [x - 0.6, y, 0], buff=0.1, color=RED, stroke_width=3)
            cmd.next_to(arr, UP, buff=0.05)
            cross = Text("✗", color=RED, font_size=30).move_to([x, y, 0])
            self.play(GrowArrow(arr), FadeIn(cmd), run_time=0.28)
            self.play(FadeIn(cross, scale=1.4), vt.animate.set_value(vals[i]), run_time=0.24)
            self.play(FadeOut(arr), FadeOut(cmd), run_time=0.12)
        self.wait(0.5)

        # collapse blind mess, show fastfind
        self.play(FadeOut(folders), FadeOut(cap),
                  *[FadeOut(m) for m in self.mobjects if isinstance(m, Text) and m.text == "✗"])
        cap2 = Text("fastfind: one grounded call", color=GREEN, font_size=28).to_edge(UP, buff=0.5)
        col[0] = GREEN
        self.play(FadeIn(cap2), vt.animate.set_value(950))
        self.play(Transform(clabel, Text("tokens (1 search call)", color=MUTED, font_size=18)
                            .next_to(counter, DOWN, buff=0.1)))

        target = RoundedRectangle(width=3.6, height=0.9, corner_radius=0.1, stroke_color=GREEN,
                                  stroke_width=3, fill_color=PANEL, fill_opacity=1).move_to([3.2, 0.4, 0])
        tpath = Text("C:\\...\\report.pdf", color=FG, font_size=22, font="monospace").move_to(target)
        call = Text('search(ext="pdf", any_of=["report"])', color=BLUE, font_size=20, font="monospace")
        arr = Arrow(agent.get_right(), target.get_left(), buff=0.1, color=GREEN, stroke_width=5)
        call.next_to(arr, UP, buff=0.12)
        self.play(GrowArrow(arr), FadeIn(call))
        self.play(Create(target), Write(tpath))
        chk = Text("✓", color=GREEN, font_size=44).next_to(target, RIGHT, buff=0.3)
        self.play(FadeIn(chk, scale=1.4))
        self.wait(1.2)
