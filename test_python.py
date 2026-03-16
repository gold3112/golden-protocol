#!/usr/bin/env python3
import requests
import math
import os
import sys
import time
import shutil
import hashlib

# サーバー設定
BASE_URL = "http://localhost:7331"

# 色と装飾
RESET  = "\033[0m"
BOLD   = "\033[1m"
DIM    = "\033[2m"
CYAN   = "\033[36m"
YELLOW = "\033[33m"
WHITE  = "\033[97m"
GRAY   = "\033[90m"
GREEN  = "\033[32m"
BLUE   = "\033[34m"
MAGENTA = "\033[35m"

def get_terminal_size():
    size = shutil.get_terminal_size((80, 24))
    return size.columns, size.lines

def label_to_angle(label):
    # ラベルごとに固有の角度を割り当てて配置を固定する
    h = hashlib.md5(label.encode()).hexdigest()
    return (int(h, 16) % 360) * (math.pi / 180)

def clear():
    # Windows/Linux両対応
    os.system('cls' if os.name == 'nt' else 'clear')

def draw_map(field, interest):
    cols, rows = get_terminal_size()
    # 描画エリアの決定 (正方形に近い形にするために調整)
    margin_y = 6
    margin_x = 4
    width = cols - margin_x
    height = rows - margin_y
    
    center_x = width // 2 + (margin_x // 2)
    center_y = height // 2 + (margin_y // 2) - 1
    
    # バッファ初期化
    canvas = [[" " for _ in range(cols)] for _ in range(rows)]
    
    near_thresh = field.get("thresholds", [0.35, 0.65])[0]
    horizon_thresh = field.get("thresholds", [0.35, 0.65])[1]
    
    # 補助線 (Near境界) の描画
    radius_near_x = int(near_thresh * (width // 2) * 0.8)
    radius_near_y = int(near_thresh * (height // 2) * 0.8)
    for a in range(0, 360, 10):
        rad = a * math.pi / 180
        x = int(center_x + radius_near_x * math.cos(rad) * 2) # ターミナルの文字幅調整
        y = int(center_y + radius_near_y * math.sin(rad))
        if 0 <= x < cols and 0 <= y < rows:
            canvas[y][x] = f"{GRAY}·{RESET}"

    # 存在の配置
    all_entities = field.get("near", []) + field.get("horizon", [])
    for ent in all_entities:
        dist = ent["distance"]
        angle = label_to_angle(ent["label"])
        
        # 距離を座標に変換 (中心からのオフセット)
        # ターミナルの文字は縦長なので、X方向を2倍にして円に見えるように調整
        offset_x = int(dist * (width // 2) * math.cos(angle) * 1.8)
        offset_y = int(dist * (height // 2) * math.sin(angle) * 0.9)
        
        x = center_x + offset_x
        y = center_y + offset_y
        
        if 0 <= x < cols and 0 <= y < rows:
            is_near = ent.get("visibility") == "Near" or dist <= near_thresh
            char = f"{BOLD}{WHITE}●{RESET}" if is_near else f"{DIM}{GRAY}○{RESET}"
            label = ent["label"]
            
            # 描画
            canvas[y][x] = char
            # ラベルを横に添える (重なりは簡易的に無視)
            label_text = f" {label}"
            for i, c in enumerate(label_text):
                if x + 1 + i < cols:
                    canvas[y][x + 1 + i] = f"{WHITE if is_near else GRAY}{c}{RESET}"

    # 中心 (自分)
    canvas[center_y][center_x] = f"{CYAN}▲{RESET}"

    # 出力
    clear()
    print(f"{BOLD}{CYAN} GOLDEN PROTOCOL - WANDER MAP {RESET}  {GRAY}pos:{field['position']}  presence:{field['presence']}{RESET}")
    print(f"{GRAY} {'━' * (cols-2)} {RESET}")
    
    for row in canvas[1:rows-4]:
        print("".join(row))
        
    print(f"{GRAY} {'━' * (cols-2)} {RESET}")
    print(f"  {CYAN}▲{RESET}:You  {BOLD}{WHITE}●{RESET}:Near  {DIM}{GRAY}○{RESET}:Horizon  {GRAY}[Interest: {interest or 'none'}]{RESET}")
    print(f"  {DIM}Enter interest to move... (q to quit){RESET}")

def main():
    interest = None
    print("Connecting to GOLDEN PROTOCOL core...")
    
    while True:
        try:
            params = {}
            if interest:
                params["interest"] = interest
            
            response = requests.get(f"{BASE_URL}/field", params=params, timeout=5)
            field = response.json()
            
            draw_map(field, interest)
            
            try:
                user_input = input("\n  > ").strip()
            except (KeyboardInterrupt, EOFError):
                break
                
            if user_input.lower() == 'q':
                break
            elif user_input:
                interest = user_input
                
        except Exception as e:
            print(f"\n{YELLOW}Connection error: {e}{RESET}")
            time.sleep(2)

if __name__ == "__main__":
    main()
