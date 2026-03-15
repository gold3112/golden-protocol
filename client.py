#!/usr/bin/env python3
"""
GOLDEN PROTOCOL — CLI prototype
散策クライアント: identity layerと連動した空間体験
"""

import requests
import sys
import os
import json
import time
import shutil

BASE_URL  = "http://localhost:7331"
ID_FILE   = os.path.expanduser("~/.golden_identity")
TERM_WIDTH = shutil.get_terminal_size((80, 24)).columns

RESET  = "\033[0m"
BOLD   = "\033[1m"
DIM    = "\033[2m"
CYAN   = "\033[36m"
YELLOW = "\033[33m"
WHITE  = "\033[97m"
GRAY   = "\033[90m"
GREEN  = "\033[32m"
BLUE   = "\033[34m"

def clear():
    os.system("clear")

def rule(char="─"):
    return char * TERM_WIDTH

def api(path, params=None, method="get", json_body=None):
    try:
        if method == "post":
            r = requests.post(f"{BASE_URL}{path}", json=json_body, timeout=5)
        else:
            r = requests.get(f"{BASE_URL}{path}", params=params, timeout=5)
        return r.json()
    except Exception:
        return None

def load_identity():
    if os.path.exists(ID_FILE):
        with open(ID_FILE) as f:
            return json.load(f)
    return None

def save_identity(data):
    with open(ID_FILE, "w") as f:
        json.dump(data, f)

def create_identity(interest=None):
    params = {}
    if interest:
        params["interest"] = interest
    data = api("/identity/new", params=params)
    if data:
        save_identity({"id": data["id"]})
    return data

def render_bar(value, width=20, fill="█", empty="░"):
    filled = int(value * width)
    return fill * filled + empty * (width - filled)

def render_near(entity):
    d = entity["distance"]
    bar = render_bar(1.0 - d, width=14)
    return f"  {BOLD}{WHITE}●{RESET} {entity['label']:<26} {CYAN}{bar}{RESET}  {GRAY}{d:.3f}{RESET}"

def render_horizon(entity):
    d = entity["distance"]
    dots = "·  " * 5
    return f"  {DIM}{GRAY}○{RESET} {DIM}{entity['label']:<26}{RESET} {GRAY}{dots}{d:.3f}{RESET}"

def render_field(field, identity, interest):
    clear()
    pos      = field["position"]
    density  = field["density"]
    presence = field["presence"]
    near     = field["near"]
    horizon  = field["horizon"]
    thresh   = field.get("thresholds", [0.35, 0.65])
    ts       = field["timestamp"][:19].replace("T", " ")

    enc_count = identity.get("encounter_count", 0) if identity else 0
    uid_short = identity["id"][:8] if identity else "anonymous"

    print()
    print(f"{BOLD}{CYAN}{rule('═')}{RESET}")
    print(f"  {BOLD}{WHITE}{pos.upper()}{RESET}  "
          f"{GRAY}presence:{presence}  "
          f"density:{render_bar(density, width=8)}  "
          f"{ts}{RESET}")

    if identity:
        print(f"  {BLUE}identity:{uid_short}  "
              f"encounters:{enc_count}{RESET}")

    print(f"{CYAN}{rule('─')}{RESET}")
    print()

    if near:
        print(f"  {BOLD}{GREEN}— NEAR —{RESET}")
        for e in near:
            print(render_near(e))
        print()

    if horizon:
        print(f"  {DIM}{GRAY}— HORIZON —{RESET}")
        for e in horizon:
            print(render_horizon(e))
        print()

    if not near and not horizon:
        print(f"  {DIM}{GRAY}  静寂。{RESET}")
        print()

    print(f"{CYAN}{rule('─')}{RESET}")
    print(f"  {GRAY}near<{thresh[0]:.2f}  horizon<{thresh[1]:.2f}  (dynamic){RESET}")
    print(f"  {GRAY}[{interest or 'default interest'}]{RESET}")
    print(f"{CYAN}{rule('─')}{RESET}")
    print()
    if identity:
        print(f"  {DIM}関心を入力→散策  /  e→このnearと出会いを記録  /  q→終了{RESET}")
    else:
        print(f"  {DIM}関心を入力→散策  /  q→終了  /  id:<text>→identity作成{RESET}")
    print()

def record_encounter(identity, field, interest_text):
    if not identity:
        return None
    near_labels = [e["label"] for e in field["near"]]
    if not near_labels:
        return None
    result = api("/encounter", method="post", json_body={
        "user_id":      identity["id"],
        "position":     field["position"],
        "near_labels":  near_labels,
        "interest_text": interest_text or "",
    })
    if result:
        identity["encounter_count"] = result["encounter_count"]
        save_identity(identity)
    return result

def main():
    identity = load_identity()
    interest = None
    last_field = None

    print(f"\n{BOLD}{CYAN}GOLDEN PROTOCOL{RESET} — connecting...\n")

    field = api("/field")
    if not field:
        print(f"{YELLOW}core serverに接続できません (:7331){RESET}")
        sys.exit(1)

    if identity:
        id_data = api(f"/identity/{identity['id']}")
        if id_data:
            identity.update(id_data)
            print(f"{GREEN}identity復元: {identity['id'][:8]}  encounters:{identity.get('encounter_count',0)}{RESET}")
        else:
            identity = None
    else:
        print(f"{GRAY}identityなし。'id:音楽' のように入力して作成できます。{RESET}")

    time.sleep(0.6)

    while True:
        params = {}
        if identity:
            params["user_id"] = identity["id"]
        if interest:
            params["interest"] = interest

        field = api("/field", params=params)
        if not field:
            print(f"\n{YELLOW}接続が切れました。{RESET}")
            break

        last_field = field
        render_field(field, identity, interest)

        try:
            user_input = input("  > ").strip()
        except (KeyboardInterrupt, EOFError):
            print(f"\n{DIM}空間から出ます。{RESET}\n")
            break

        if user_input.lower() == "q":
            print(f"\n{DIM}空間から出ます。{RESET}\n")
            break

        elif user_input.lower() == "e":
            # nearと出会いを記録
            result = record_encounter(identity, last_field, interest)
            if result:
                enc = result.get("encounter_count", 0)
                print(f"  {GREEN}記録しました (encounters: {enc}){RESET}")
                time.sleep(0.5)
            else:
                print(f"  {GRAY}nearに存在がないか、identityがありません{RESET}")
                time.sleep(0.5)

        elif user_input.lower().startswith("id:"):
            # identity作成
            seed = user_input[3:].strip()
            data = create_identity(seed or None)
            if data:
                identity = data
                print(f"  {GREEN}identity作成: {data['id'][:8]}{RESET}")
                time.sleep(0.6)

        elif user_input:
            interest = user_input

if __name__ == "__main__":
    main()
