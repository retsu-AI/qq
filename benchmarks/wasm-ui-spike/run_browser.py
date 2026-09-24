#!/usr/bin/env python3
"""Drives each spike page in the already-running Chrome (CDP on :29229),
kicks off `--sessions` streaming runs through `drive.sh`, waits for the stream
to finish, and prints the page's own status-strip metrics plus a screenshot
per page.

    python3 run_browser.py --server http://127.0.0.1:9077 --workspace ~/spike-ws
"""

import argparse
import asyncio
import json
import os
import re
import subprocess
import time
from pathlib import Path

from playwright.async_api import async_playwright

HERE = Path(__file__).resolve().parent


def token(path):
    return re.search(r'token:"([^"]+)"', Path(path).read_text()).group(1)


async def measure(page, name, url, drive, settle_s, shots):
    await page.goto(url, wait_until="load")
    await page.wait_for_selector(".status", timeout=30_000)
    # Wait until the feed is live before starting the runs.
    await page.wait_for_function(
        "() => document.querySelector('.status')?.innerText.includes('live')", timeout=30_000
    )
    started = time.time()
    subprocess.run(drive, check=True, stdout=subprocess.DEVNULL)
    # Focus the newest session so the transcript renders while it streams.
    await asyncio.sleep(1.0)
    await page.click(".row")
    # Runs are finished when the event counter stops moving.
    last, stable_since = -1, time.time()
    while time.time() - stable_since < settle_s:
        events = await page.evaluate(
            "() => parseInt(document.querySelector('.status').innerText.match(/events (\\d+)/)[1])"
        )
        if events != last:
            last, stable_since = events, time.time()
        await asyncio.sleep(0.25)
    status = await page.evaluate("() => document.querySelector('.status').innerText")
    heap = await page.evaluate("() => performance.memory ? performance.memory.usedJSHeapSize : null")
    nodes = await page.evaluate("() => document.getElementsByTagName('*').length")
    transcript = await page.evaluate("() => document.querySelector('.transcript').innerText.length")
    await page.screenshot(path=str(shots / f"{name}.png"))
    return {
        "candidate": name,
        "status": status.replace("\n", " "),
        "wall_s": round(time.time() - started - settle_s, 1),
        "js_heap_bytes": heap,
        "dom_nodes": nodes,
        "transcript_chars": transcript,
    }


async def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--server", default="http://127.0.0.1:9077")
    parser.add_argument("--pages", default="http://127.0.0.1:8090")
    parser.add_argument("--workspace", required=True)
    parser.add_argument("--sessions", type=int, default=3)
    parser.add_argument("--settle", type=float, default=3.0)
    parser.add_argument("--cdp", default="http://localhost:29229")
    parser.add_argument("--shots", default=str(HERE / "dist" / "shots"))
    parser.add_argument(
        "--server-ron", default=os.path.expanduser("~/.local/share/qq/runtime/server.ron")
    )
    args = parser.parse_args()
    shots = Path(args.shots)
    shots.mkdir(parents=True, exist_ok=True)
    workspace = os.path.abspath(os.path.expanduser(args.workspace))
    # The credential rides in the query string only because this is a
    # throwaway spike page on loopback; U2 stores it in IndexedDB.
    query = f"?server={args.server}&credential={token(args.server_ron)}&workspace={workspace}"
    drive = [str(HERE / "drive.sh"), args.server, workspace, str(args.sessions)]

    results = []
    async with async_playwright() as pw:
        browser = await pw.chromium.connect_over_cdp(args.cdp)
        context = browser.contexts[0]
        page = await context.new_page()
        for name in ("baseline", "leptos", "dioxus"):
            url = f"{args.pages}/{name}/index.html{query}"
            results.append(await measure(page, name, url, drive, args.settle, shots))
        # Remote-loading host: both remotes side by side, loaded as ES modules.
        await page.goto(f"{args.pages}/index.html{query}", wait_until="load")
        await page.wait_for_function(
            "() => document.querySelectorAll('.status').length === 2", timeout=30_000
        )
        await asyncio.sleep(1.0)
        header = await page.evaluate("() => document.querySelector('header').innerText")
        await page.screenshot(path=str(shots / "host.png"))
        await page.click("#unmount-leptos")
        await page.click("#unmount-dioxus")
        await asyncio.sleep(0.5)
        remaining = await page.evaluate("() => document.querySelectorAll('.status').length")
        await page.screenshot(path=str(shots / "host-unmounted.png"))
        results.append({"host": header.replace("\n", " "), "status_nodes_after_unmount": remaining})
        await page.close()
    print(json.dumps(results, indent=2))


if __name__ == "__main__":
    asyncio.run(main())
