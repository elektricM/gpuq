#!/usr/bin/env python3
"""Render the overview video from scenes.html: python3 docs/video/render.py

Writes docs/overview.mp4 (1280x720) and docs/overview-square.mp4 (1080x1080).
Needs Python Playwright with its Chromium (pip install playwright; playwright install chromium)
and ffmpeg with libx264.
"""
import pathlib
import subprocess
import tempfile

from playwright.sync_api import sync_playwright

HERE = pathlib.Path(__file__).resolve().parent
DOCS = HERE.parent
FPS = 30
OUTPUTS = [(1280, 720, DOCS / 'overview.mp4'), (1080, 1080, DOCS / 'overview-square.mp4')]


def render(browser, w, h, out):
    page = browser.new_page(viewport={'width': w, 'height': h})
    page.goto(f'{(HERE / "scenes.html").as_uri()}?w={w}&h={h}')
    frames = round(page.evaluate('TOTAL') * FPS)
    with tempfile.TemporaryDirectory() as tmp:
        for i in range(frames):
            page.evaluate(f'draw({i / FPS})')
            page.screenshot(path=f'{tmp}/f{i:04d}.png')
        subprocess.run(['ffmpeg', '-loglevel', 'error', '-y', '-framerate', str(FPS), '-i', f'{tmp}/f%04d.png',
                        '-c:v', 'libx264', '-preset', 'slow', '-crf', '22', '-pix_fmt', 'yuv420p',
                        '-movflags', '+faststart', str(out)], check=True)
    page.close()
    print(f'{out.relative_to(DOCS.parent)}: {w}x{h}, {frames} frames')


with sync_playwright() as p:
    browser = p.chromium.launch(args=['--disable-lcd-text'])
    for w, h, out in OUTPUTS:
        render(browser, w, h, out)
    browser.close()
