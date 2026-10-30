import os
import math
from PIL import Image, ImageDraw, ImageFont, ImageFilter

os.makedirs('assets', exist_ok=True)

# 1. Generate high-res Icon (256x256)
def generate_icon(size=256):
    img = Image.new('RGBA', (size, size), (0, 0, 0, 0))
    pixels = img.load()
    cx, cy = 0.42, 0.42
    r_out = 0.34
    r_in = 0.20 if size <= 16.0 else 0.235
    ax, ay, bx, by = 0.66, 0.66, 0.90, 0.90
    vx, vy = bx - ax, by - ay
    v_len_sq = vx * vx + vy * vy
    handle_r = 0.10 if size <= 16.0 else 0.085

    for py in range(size):
        for px in range(size):
            ring = 0.0
            handle = 0.0
            for sy in range(4):
                for sx in range(4):
                    x = (px + (sx + 0.5) / 4.0) / size
                    y = (py + (sy + 0.5) / 4.0) / size
                    d = math.hypot(x - cx, y - cy)
                    if r_in <= d <= r_out:
                        ring += 1.0 / 16.0
                    t = max(0.0, min(1.0, ((x - ax) * vx + (y - ay) * vy) / v_len_sq))
                    qx, qy = ax + t * vx, ay + t * vy
                    hd = math.hypot(x - qx, y - qy)
                    if hd <= handle_r and d > r_in:
                        handle += 1.0 / 16.0
            a = min(1.0, ring + handle)
            if a > 0:
                if handle > ring:
                    r, g, b = 0xD9, 0x5A, 0x0B
                else:
                    r, g, b = 0xFF, 0x8A, 0x1F
                pixels[px, py] = (r, g, b, int(round(a * 255)))
    return img

icon_img = generate_icon(256)
icon_img.save('assets/icon.png')
print("Saved assets/icon.png")

# Fonts helper
font_dir = os.path.join(os.environ.get('WINDIR', 'C:\\Windows'), 'Fonts')
def get_font(name, size):
    path = os.path.join(font_dir, name)
    if os.path.exists(path):
        return ImageFont.truetype(path, size)
    return ImageFont.load_default()

font_ui = get_font('segoeui.ttf', 13)
font_ui_bold = get_font('seguisb.ttf', 13)
font_ui_title = get_font('seguisb.ttf', 12)
font_ui_large = get_font('seguisb.ttf', 15)
font_code = get_font('consola.ttf', 13)
font_status = get_font('segoeui.ttf', 12)

# 2. Generate Window UI Preview Mockup (960x600)
def generate_preview():
    W, H = 1000, 640
    # Create smooth dark backdrop with subtle radial glow
    bg = Image.new('RGBA', (W, H), (15, 17, 23, 255))
    draw_bg = ImageDraw.Draw(bg)

    # Accent glow in background
    for r in range(300, 0, -5):
        alpha = int(22 * (1.0 - r / 300.0))
        draw_bg.ellipse([(W//2 - r, 200 - r//2), (W//2 + r, 200 + r//2)], fill=(255, 138, 31, alpha))

    # Window geometry
    win_x, win_y = 50, 45
    win_w, win_h = 900, 550
    radius = 10

    # Draw window shadow
    shadow = Image.new('RGBA', (W, H), (0, 0, 0, 0))
    sdraw = ImageDraw.Draw(shadow)
    sdraw.rounded_rectangle([win_x - 6, win_y - 2, win_x + win_w + 6, win_y + win_h + 12], radius=radius+4, fill=(0, 0, 0, 140))
    shadow = shadow.filter(ImageFilter.GaussianBlur(16))
    bg.alpha_composite(shadow)

    # Window surface
    win = Image.new('RGBA', (win_w, win_h), (18, 18, 18, 255))
    wdraw = ImageDraw.Draw(win)

    # Title bar (height 36)
    title_h = 36
    wdraw.rectangle([0, 0, win_w, title_h], fill=(22, 22, 22, 255))
    
    # Title bar icon
    small_icon = icon_img.resize((18, 18), Image.Resampling.LANCZOS)
    win.alpha_composite(small_icon, (14, 9))
    wdraw.text((38, 10), "Nothing", font=font_ui_title, fill=(210, 210, 210))

    # Window controls (min, max, close)
    # Minimize
    wdraw.line([(win_w - 110, 18), (win_w - 98, 18)], fill=(160, 160, 160), width=1)
    # Maximize
    wdraw.rectangle([win_w - 74, 13, win_w - 62, 23], outline=(160, 160, 160), width=1)
    # Close
    wdraw.line([(win_w - 38, 13), (win_w - 28, 23)], fill=(160, 160, 160), width=1)
    wdraw.line([(win_w - 28, 13), (win_w - 38, 23)], fill=(160, 160, 160), width=1)

    # Menu bar (height 26)
    menu_y = title_h
    menus = ["File", "Edit", "View", "Search", "Bookmarks", "Tools", "Help"]
    mx = 14
    for m in menus:
        wdraw.text((mx, menu_y + 4), m, font=font_ui, fill=(200, 200, 200))
        mx += int(wdraw.textlength(m, font=font_ui)) + 18

    # Search bar container (height 44)
    search_box_y = menu_y + 28
    wdraw.rectangle([12, search_box_y, win_w - 12, search_box_y + 34], fill=(10, 10, 10), outline=(48, 48, 48), width=1)

    # Search magnifier inside box
    wdraw.text((24, search_box_y + 8), "🔍", font=font_ui, fill=(255, 138, 31))
    # Query text
    query_str = "chernobyl *.mkv"
    wdraw.text((48, search_box_y + 8), query_str, font=font_code, fill=(240, 240, 240))
    # Caret
    caret_x = 48 + int(wdraw.textlength(query_str, font=font_code)) + 2
    wdraw.line([(caret_x, search_box_y + 7), (caret_x, search_box_y + 25)], fill=(255, 138, 31), width=2)

    # Search badges on the right
    wdraw.rounded_rectangle([win_w - 190, search_box_y + 6, win_w - 116, search_box_y + 26], radius=4, fill=(30, 30, 30), outline=(50, 50, 50))
    wdraw.text((win_w - 180, search_box_y + 8), "Match Path", font=font_status, fill=(160, 160, 160))

    wdraw.rounded_rectangle([win_w - 108, search_box_y + 6, win_w - 24, search_box_y + 26], radius=4, fill=(35, 24, 15), outline=(255, 138, 31, 120))
    wdraw.text((win_w - 98, search_box_y + 8), "0.7 ms latency", font=font_status, fill=(255, 150, 45))

    # Header Row (height 28)
    hdr_y = search_box_y + 44
    wdraw.rectangle([0, hdr_y, win_w, hdr_y + 28], fill=(16, 16, 16))
    wdraw.line([(0, hdr_y + 28), (win_w, hdr_y + 28)], fill=(40, 40, 40), width=1)

    cols = [
        ("Name ▾", 18, 380),
        ("Path", 398, 310),
        ("Size", 718, 90),
        ("Date Modified", 818, 160)
    ]
    for name, cx_pos, _ in cols:
        wdraw.text((cx_pos, hdr_y + 5), name, font=font_ui_bold, fill=(180, 180, 180))

    # Results table area
    row_y = hdr_y + 29
    row_h = 30
    items = [
        ("Chernobyl.S01E01.123.45.1080p.mkv", "D:\\Media\\Shows\\Chernobyl\\Season 1", "2.41 GB", "2024-05-14 18:32", True),
        ("Chernobyl.S01E02.Please.Remain.Calm.1080p.mkv", "D:\\Media\\Shows\\Chernobyl\\Season 1", "2.38 GB", "2024-05-14 18:35", False),
        ("Chernobyl.S01E03.Open.Wide.O.Earth.1080p.mkv", "D:\\Media\\Shows\\Chernobyl\\Season 1", "2.55 GB", "2024-05-14 18:38", False),
        ("Chernobyl.S01E04.The.Happiness.Of.All.Mankind.1080p.mkv", "D:\\Media\\Shows\\Chernobyl\\Season 1", "2.49 GB", "2024-05-14 18:41", False),
        ("Chernobyl.S01E05.Vichnaya.Pamyat.1080p.mkv", "D:\\Media\\Shows\\Chernobyl\\Season 1", "2.62 GB", "2024-05-14 18:45", False),
        ("The.Real.Chernobyl.Documentary.720p.mkv", "D:\\Media\\Documentaries", "1.12 GB", "2023-11-20 21:10", False),
        ("Chernobyl.Diaries.2012.BRRip.mkv", "D:\\Media\\Movies", "1.75 GB", "2022-09-08 14:02", False),
        ("Chernobyl.Abyss.2021.1080p.mkv", "D:\\Media\\Movies", "2.10 GB", "2023-03-12 11:24", False),
        ("Chernobylite.Game.Cinematic.Trailer.1080p.mkv", "C:\\Users\\Nihil\\Videos", "345 MB", "2024-01-05 16:15", False),
        ("Voices.From.Chernobyl.Audiobook.Part1.mkv", "D:\\Audiobooks", "420 MB", "2022-04-18 09:50", False),
        ("Voices.From.Chernobyl.Audiobook.Part2.mkv", "D:\\Audiobooks", "415 MB", "2022-04-18 10:12", False),
    ]

    for name, path, size_str, date_str, is_selected in items:
        if is_selected:
            wdraw.rectangle([0, row_y, win_w, row_y + row_h], fill=(38, 55, 78)) # selected row
            text_color = (255, 255, 255)
            path_color = (190, 215, 245)
        else:
            text_color = (220, 220, 220)
            path_color = (130, 130, 130)

        # File icon
        wdraw.rounded_rectangle([18, row_y + 7, 32, row_y + 22], radius=2, fill=(180, 50, 60) if 'mkv' in name else (60, 120, 180))
        wdraw.text((20, row_y + 7), "▶", font=font_status, fill=(255, 255, 255))

        # Columns
        wdraw.text((40, row_y + 6), name, font=font_ui, fill=text_color)
        wdraw.text((398, row_y + 6), path, font=font_ui, fill=path_color)
        wdraw.text((718, row_y + 6), size_str, font=font_ui, fill=text_color)
        wdraw.text((818, row_y + 6), date_str, font=font_ui, fill=path_color)

        row_y += row_h

    # Status Bar at bottom (height 28)
    status_y = win_h - 28
    wdraw.rectangle([0, status_y, win_w, win_h], fill=(14, 14, 14))
    wdraw.line([(0, status_y), (win_w, status_y)], fill=(32, 32, 32), width=1)

    wdraw.text((14, status_y + 6), "11 items   ·   1 selected (2.41 GB)", font=font_status, fill=(170, 170, 170))
    status_r = "Indexed: 1,142,890 files   ·   Scan: 0.72 ms   ·   USN Journal: Live"
    rlen = int(wdraw.textlength(status_r, font=font_status))
    wdraw.text((win_w - rlen - 16, status_y + 6), status_r, font=font_status, fill=(255, 145, 40))

    # Mask window corners with round rectangle
    mask = Image.new('L', (win_w, win_h), 0)
    mdraw = ImageDraw.Draw(mask)
    mdraw.rounded_rectangle([0, 0, win_w, win_h], radius=radius, fill=255)

    # Window border outline
    border = Image.new('RGBA', (win_w, win_h), (0, 0, 0, 0))
    bdraw = ImageDraw.Draw(border)
    bdraw.rounded_rectangle([0, 0, win_w - 1, win_h - 1], radius=radius, outline=(60, 60, 60, 200), width=1)

    win.putalpha(mask)
    win.alpha_composite(border)

    bg.alpha_composite(win, (win_x, win_y))
    bg.convert('RGB').save('assets/nothing-preview.png', quality=95)
    print("Saved assets/nothing-preview.png")

generate_preview()

# 3. Generate Architecture Diagram (960x420)
def generate_architecture():
    W, H = 960, 420
    im = Image.new('RGBA', (W, H), (18, 20, 28, 255))
    draw = ImageDraw.Draw(im)

    # Title
    font_h1 = get_font('seguisb.ttf', 18)
    font_h2 = get_font('seguisb.ttf', 13)
    font_txt = get_font('segoeui.ttf', 11)
    font_mono = get_font('consola.ttf', 11)

    draw.text((36, 22), "High-Performance Architecture Pipeline", font=font_h1, fill=(255, 255, 255))
    draw.text((36, 48), "Direct NTFS Master File Table parsing with SIMD substring searching over a lock-free virtual list", font=font_txt, fill=(160, 165, 180))

    # Pipeline Boxes:
    # 1. NTFS Source -> 2. In-Memory Arena & Index -> 3. SIMD Engine -> 4. Named Pipe IPC -> 5. Win32 Virtual UI
    boxes = [
        ("NTFS Master File Table", ["8 parallel raw readers", "Unbuffered disk I/O (~450 MB/s)", "USN Journal delta stream", "Bypasses directory walking"], 36, 95, 165, 260, (255, 138, 31)),
        ("Struct-of-Arrays Index", ["Contiguous name arena", "Case-folded twin arena", "Zero-alloc offset index", "Snapshotted to index.bin"], 220, 95, 165, 260, (0, 176, 116)),
        ("SIMD Query Engine", ["Multi-core memmem scan", "Pre-sorted order bitsets", "Path matching bitmasks", "Incremental narrowing"], 404, 95, 165, 260, (75, 150, 255)),
        ("Named Pipe IPC", ["Shared memory protocol", "Single-frame roundtrips", "Background worker thread", "Lock-free synchronization"], 588, 95, 165, 260, (180, 110, 255)),
        ("Win32 Virtual ListView", ["LVS_OWNERDATA list", "Renders visible ~40 rows", "OLED / Dark / Light theme", "Global Win+Alt+Space"], 772, 95, 155, 260, (255, 90, 130))
    ]

    for title, points, bx, by, bw, bh, color in boxes:
        # Box background
        draw.rounded_rectangle([bx, by, bx + bw, by + bh], radius=8, fill=(26, 29, 40), outline=(color[0], color[1], color[2], 120), width=1)
        # Top color accent bar
        draw.rounded_rectangle([bx, by, bx + bw, by + 6], radius=2, fill=color)

        draw.text((bx + 12, by + 16), title, font=font_h2, fill=(240, 240, 240))
        draw.line([(bx + 12, by + 40), (bx + bw - 12, by + 40)], fill=(45, 50, 68), width=1)

        py = by + 52
        for pt in points:
            draw.text((bx + 12, py), "•", font=font_txt, fill=color)
            draw.text((bx + 22, py), pt, font=font_txt, fill=(185, 190, 205))
            py += 36

        # Draw connecting arrow to next box
        next_x = bx + bw
        if next_x < 750:
            arrow_center_y = by + bh // 2
            draw.line([(next_x + 3, arrow_center_y), (next_x + 16, arrow_center_y)], fill=(120, 130, 150), width=2)
            draw.polygon([(next_x + 18, arrow_center_y), (next_x + 13, arrow_center_y - 4), (next_x + 13, arrow_center_y + 4)], fill=(180, 190, 210))

    # Bottom summary bar
    draw.rounded_rectangle([36, 375, W - 36, 402], radius=6, fill=(23, 26, 36), outline=(40, 45, 60))
    draw.text((50, 382), "⚡ Cold scan: 3.3 s for 1.1M files   |   Warm restart: 86 ms   |   Query latency: 0.7–0.9 ms   |   Memory: ~96 bytes/file", font=font_mono, fill=(255, 150, 45))

    im.convert('RGB').save('assets/architecture.png', quality=95)
    print("Saved assets/architecture.png")

generate_architecture()
