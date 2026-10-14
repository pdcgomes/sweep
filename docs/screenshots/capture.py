"""Drives `sweep demo` in a pty, captures the screen at chosen moments, and
renders each capture as a PNG framed like a macOS terminal window.

Box-drawing lines and bar blocks are drawn as shapes so they join without
gaps; everything else is text, in SF Mono with fallbacks for symbols.

    cargo build --release
    pip install pyte pillow fonttools
    python3 docs/screenshots/capture.py
"""
import fcntl
import os
import pty
import select
import struct
import sys
import termios
import time

import pyte
from fontTools.ttLib import TTFont
from PIL import Image, ImageDraw, ImageFilter, ImageFont

HERE = os.path.dirname(os.path.abspath(__file__))
SWEEP = os.path.join(HERE, "..", "..", "target", "release", "sweep")
TERMFONTS = "/System/Applications/Utilities/Terminal.app/Contents/Resources/Fonts"
BG = (0x1A, 0x1B, 0x26)
FG = (0xC0, 0xCA, 0xF5)

# ---------------------------------------------------------------- capture


class Session:
    def __init__(self, cols, rows, args=("demo",)):
        self.cols, self.rows = cols, rows
        self.screen = pyte.Screen(cols, rows)
        self.stream = pyte.ByteStream(self.screen)
        pid, fd = pty.fork()
        if pid == 0:
            fcntl.ioctl(0, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
            env = {k: v for k, v in os.environ.items() if k != "NO_COLOR"}
            env.update(TERM="xterm-256color", COLORTERM="truecolor")
            os.execve(SWEEP, [SWEEP, *args], env)
        self.pid, self.fd = pid, fd
        self.pump(1.0)

    def pump(self, seconds):
        end = time.time() + seconds
        while True:
            left = end - time.time()
            if left <= 0:
                break
            r, _, _ = select.select([self.fd], [], [], left)
            if r:
                try:
                    data = os.read(self.fd, 65536)
                except OSError:
                    break
                if not data:
                    break
                self.stream.feed(data)

    def keys(self, *keys, wait=0.35):
        named = {"down": "\x1b[B", "up": "\x1b[A", "right": "\x1b[C", "left": "\x1b[D", "enter": "\r", "space": " "}
        for k in keys:
            os.write(self.fd, named.get(k, k).encode())
            self.pump(wait)

    def grab(self):
        return {y: dict(self.screen.buffer[y]) for y in range(self.rows)}

    def close(self):
        try:
            os.write(self.fd, b"q")
            self.pump(0.5)
            os.kill(self.pid, 9)
        except OSError:
            pass


# ---------------------------------------------------------------- fonts


class Fonts:
    def __init__(self, size):
        regular = [
            (f"{TERMFONTS}/SF-Mono-Regular.otf", 0),
            ("/System/Library/Fonts/Menlo.ttc", 0),
            ("/System/Library/Fonts/Apple Symbols.ttf", 0),
            ("/System/Library/Fonts/Supplemental/Arial Unicode.ttf", 0),
        ]
        bold = [(f"{TERMFONTS}/SF-Mono-Bold.otf", 0), ("/System/Library/Fonts/Menlo.ttc", 1)] + regular[2:]
        self.size = size
        self.sets = {False: self._load(regular), True: self._load(bold)}
        primary = self.sets[False][0][0]
        self.cw = round(primary.getlength("M"))
        self.ch = round(size * 1.28)

    def _load(self, specs):
        out = []
        for path, index in specs:
            if not os.path.exists(path):
                continue
            font = ImageFont.truetype(path, self.size, index=index)
            cmap = set(TTFont(path, fontNumber=index, lazy=True).getBestCmap())
            out.append((font, cmap))
        return out

    def pick(self, ch, bold):
        for font, cmap in self.sets[bold]:
            if ord(ch) in cmap:
                return font
        return self.sets[bold][0][0]


# ---------------------------------------------------------------- render

NAMED = {
    "black": (0x15, 0x16, 0x1E), "red": (0xF7, 0x76, 0x8E), "green": (0x9E, 0xCE, 0x6A), "brown": (0xE0, 0xAF, 0x68),
    "yellow": (0xE0, 0xAF, 0x68), "blue": (0x7A, 0xA2, 0xF7), "magenta": (0xBB, 0x9A, 0xF7), "cyan": (0x7D, 0xCF, 0xFF),
    "white": (0xA9, 0xB1, 0xD6),
}


def colour(c, fallback):
    if c == "default":
        return fallback
    if c in NAMED:
        return NAMED[c]
    try:
        return tuple(int(c[i:i + 2], 16) for i in (0, 2, 4))
    except (ValueError, TypeError):
        return fallback


def mix(a, b, t):
    return tuple(round(x * (1 - t) + y * t) for x, y in zip(a, b))


LIGHT = {"─": "lr", "│": "ud", "╭": "rd", "╮": "ld", "╰": "ur", "╯": "ul", "┌": "rd", "┐": "ld", "└": "ur", "┘": "ul",
         "├": "udr", "┤": "udl", "┬": "lrd", "┴": "lru", "┼": "udlr"}
ROUNDED = set("╭╮╰╯")
HEAVY = {"━": "lr", "┃": "ud"}
SHADES = {"█": 1.0, "▓": 0.72, "▒": 0.48, "░": 0.22}


def draw_box(d, ch, x, y, cw, chh, fg, width):
    cx, cy = x + cw // 2, y + chh // 2
    arms = LIGHT.get(ch) or HEAVY.get(ch)
    if ch in ROUNDED:
        r = min(cw, chh) // 2
        if ch == "╭":
            d.arc([cx, cy, cx + 2 * r, cy + 2 * r], 180, 270, fill=fg, width=width)
            d.line([cx + r, cy, x + cw, cy], fill=fg, width=width)
            d.line([cx, cy + r, cx, y + chh], fill=fg, width=width)
        elif ch == "╮":
            d.arc([cx - 2 * r, cy, cx, cy + 2 * r], 270, 360, fill=fg, width=width)
            d.line([x, cy, cx - r, cy], fill=fg, width=width)
            d.line([cx, cy + r, cx, y + chh], fill=fg, width=width)
        elif ch == "╰":
            d.arc([cx, cy - 2 * r, cx + 2 * r, cy], 90, 180, fill=fg, width=width)
            d.line([cx + r, cy, x + cw, cy], fill=fg, width=width)
            d.line([cx, y, cx, cy - r], fill=fg, width=width)
        else:
            d.arc([cx - 2 * r, cy - 2 * r, cx, cy], 0, 90, fill=fg, width=width)
            d.line([x, cy, cx - r, cy], fill=fg, width=width)
            d.line([cx, y, cx, cy - r], fill=fg, width=width)
        return
    half = width // 2
    if "l" in arms:
        d.rectangle([x, cy - half, cx + half, cy - half + width - 1], fill=fg)
    if "r" in arms:
        d.rectangle([cx - half, cy - half, x + cw, cy - half + width - 1], fill=fg)
    if "u" in arms:
        d.rectangle([cx - half, y, cx - half + width - 1, cy + half], fill=fg)
    if "d" in arms:
        d.rectangle([cx - half, cy - half, cx - half + width - 1, y + chh], fill=fg)


def render_terminal(buf, cols, rows, fonts):
    cw, chh = fonts.cw, fonts.ch
    img = Image.new("RGB", (cols * cw, rows * chh), BG)
    d = ImageDraw.Draw(img)
    for yy in range(rows):
        line = buf.get(yy, {})
        for xx in range(cols):
            cell = line.get(xx)
            if cell is None:
                continue
            fg = colour(cell.fg, FG)
            bg = colour(cell.bg, BG)
            if cell.reverse:
                fg, bg = bg, fg
            x, y = xx * cw, yy * chh
            if bg != BG:
                d.rectangle([x, y, x + cw - 1, y + chh - 1], fill=bg)
            ch = cell.data
            if not ch or ch in (" ", "\xa0"):
                continue
            if ch in SHADES:
                d.rectangle([x, y, x + cw - 1, y + chh - 1], fill=mix(bg, fg, SHADES[ch]))
            elif ch in LIGHT:
                draw_box(d, ch, x, y, cw, chh, fg, 2)
            elif ch in HEAVY:
                draw_box(d, ch, x, y, cw, chh, fg, 5)
            else:
                font = fonts.pick(ch, cell.bold)
                ascent, descent = font.getmetrics()
                w = font.getlength(ch)
                gx = x + (cw - w) / 2 if w > cw * 1.05 or font is not fonts.sets[cell.bold][0][0] else x
                gy = y + (chh - (ascent + descent)) / 2
                d.text((gx, gy), ch, font=font, fill=fg)
    return img


def framed(term, title="sweep"):
    pad, bar, radius, margin = 28, 64, 22, 70
    w, h = term.width + 2 * pad, term.height + bar + pad
    canvas = Image.new("RGBA", (w + 2 * margin, h + 2 * margin), (0, 0, 0, 0))
    shadow = Image.new("RGBA", canvas.size, (0, 0, 0, 0))
    ImageDraw.Draw(shadow).rounded_rectangle([margin, margin + 18, margin + w, margin + h + 18], radius, fill=(0, 0, 0, 120))
    shadow = shadow.filter(ImageFilter.GaussianBlur(28))
    canvas.alpha_composite(shadow)
    win = Image.new("RGBA", (w, h), (0, 0, 0, 0))
    wd = ImageDraw.Draw(win)
    wd.rounded_rectangle([0, 0, w - 1, h - 1], radius, fill=BG + (255,), outline=(0x2F, 0x33, 0x4D, 255), width=2)
    wd.rounded_rectangle([1, 1, w - 2, bar], radius, fill=(0x16, 0x16, 0x1E, 255))
    wd.rectangle([1, bar - radius, w - 2, bar], fill=(0x16, 0x16, 0x1E, 255))
    wd.line([1, bar, w - 2, bar], fill=(0x2F, 0x33, 0x4D, 255), width=2)
    for i, c in enumerate([(0xFF, 0x5F, 0x57), (0xFE, 0xBC, 0x2E), (0x28, 0xC8, 0x40)]):
        cx = 34 + i * 40
        wd.ellipse([cx - 12, bar // 2 - 12, cx + 12, bar // 2 + 12], fill=c + (255,))
    tf = ImageFont.truetype("/System/Library/Fonts/SFNS.ttf", 26) if os.path.exists("/System/Library/Fonts/SFNS.ttf") else None
    if tf:
        tw = wd.textlength(title, font=tf)
        wd.text(((w - tw) / 2, bar / 2 - 15), title, font=tf, fill=(0x73, 0x7A, 0xA2, 255))
    win.paste(term, (pad, bar + pad // 2))
    mask = Image.new("L", (w, h), 0)
    ImageDraw.Draw(mask).rounded_rectangle([0, 0, w - 1, h - 1], radius, fill=255)
    canvas.paste(win, (margin, margin), mask)
    return canvas


def save(buf, cols, rows, out, fonts):
    framed(render_terminal(buf, cols, rows, fonts)).save(out)
    print(out)


def chars(buf):
    return sorted({c.data for line in buf.values() for c in line.values() if c.data and ord(c.data[0]) > 127})


if __name__ == "__main__":
    out = sys.argv[1] if len(sys.argv) > 1 else HERE
    os.makedirs(out, exist_ok=True)
    cols, rows = 140, 38
    fonts = Fonts(28)
    s = Session(cols, rows)
    shots = {}
    shots["overview"] = s.grab()
    s.keys("right", "down", "right", "down")   # Git worktrees › acme-web › acme-web-checkout
    shots["details"] = s.grab()
    s.keys("left", "left", "left", "left")     # back to the top row, closed
    s.keys("a", wait=0.6)            # mark every Safe item
    shots["marked"] = s.grab()
    s.keys("enter", wait=0.6)
    shots["confirm"] = s.grab()
    s.keys("y", wait=0.2)
    s.pump(7)
    shots["done"] = s.grab()
    del shots["marked"]
    s.close()
    seen = set()
    for name, buf in shots.items():
        seen.update(chars(buf))
        save(buf, cols, rows, f"{out}/{name}.png", fonts)
    print("non-ASCII:", "".join(sorted(seen)))
