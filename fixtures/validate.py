#!/usr/bin/env python3
"""校验 fixtures/ 下的样本是否符合 schema/ 下对应的 Schema（JSON Schema Draft 2020-12）。

样本发现：
  - fixtures/ 顶层与子目录（protocol/、config/ 等）中的 <schema>.valid[-后缀].json 必须通过 schema/<schema>.schema.json；
    <schema>.invalid[-后缀].json 必须失败。子目录里不符合命名的 .json（如 git/、render/ 的数据）忽略；
    顶层不符合命名的 .json 报错。
  - fixtures/config/ 目录下的样本映射为 config.v1，文件名以 valid 或 invalid 开头决定期望结果
    （例如 fixtures/config/invalid-dangling-ref.json 为 config.v1 的无效样本）。

附加检查（JSON Schema 无法表达的部分）：
  - config.v0：layout 引用的模块 ID 必须存在于 modules。
  - config.v1：layout 引用的模块 ID 必须存在于本文件的 modules，或内置配置 src/config/builtin.json 的 modules
    （用户覆盖文件只写差异部分，可引用内置模块）。
  - config.v1：palette 必须是内置调色板（src/config/palettes/*.json）或本文件 palettes 中定义的名字。
  - shell-frame.v1 的 RES：每个 L/R/T 行的 UTF-8 字节数不超过 4000（MAX_LINE_BYTES）。
  - agent-state：title ≤192、msg ≤2048、origin_session ≤128、scope.id ≤4096 个 UTF-8 字节（AGENT_BYTE_LIMITS；
    OSC 7501 按字节计，JSON Schema 的 maxLength 按字符计）。

内置配置：src/config/builtin.json 按 config.v1 校验；src/config/presets/*.json 按 config.v1 校验（预设文件省略
schema_version，本脚本校验前补上 schema_version: 1，与加载器合并层的行为一致）。预设中的 layout 引用同样做跨引用检查。
内置调色板：src/config/palettes/*.json 按 config.v1 的 $defs/palette 校验，并检查每个都定义了全部角色色（ROLES，
与 src/config/palette.rs 一致）、background 与 foreground，且值都是 #rrggbb。

线路样本 fixtures/protocol/wire-samples.txt：按空行分帧，逐帧解码（按制表符切分，cwd/env 反转义）为
shell-frame.v1 的逻辑对象后校验为有效；并断言 REQ 的 cwd 反转义结果同时含制表符与反斜杠。

覆盖规则：
  - 每个 schema 至少一个有效与一个无效样本；
  - shell-frame.v1 的每种帧（HELLO、REQ、RES、BYE）至少一个有效与一个无效 JSON 样本，且线路样本覆盖四种帧。
    无效样本的帧类型由文件名中 invalid- 之后的第一段决定（如 shell-frame.v1.invalid-req-cwd-nul.json 为 req）。

依赖：jsonschema（不自动安装，缺失时退出码 2）。
用法：python3 fixtures/validate.py
"""
import importlib.metadata
import json
import os
import re
import sys
from collections import defaultdict

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
SCHEMA_DIR = os.path.join(ROOT, "schema")
FIXTURE_DIR = os.path.dirname(os.path.abspath(__file__))
PROTOCOL_DIR = os.path.join(FIXTURE_DIR, "protocol")
WIRE = os.path.join(PROTOCOL_DIR, "wire-samples.txt")
NAME_RE = re.compile(r"^(?P<schema>.+?)\.(?P<kind>valid|invalid)(?:-[a-z0-9-]+)?\.json$")
SCHEMA_SUFFIX = ".schema.json"
# 子目录到 schema 的映射：目录中的样本按文件名前缀 valid/invalid 判定期望结果。
DIR_SCHEMA = {"config": "config.v1"}
SHELL_SCHEMA = "shell-frame.v1"
FRAMES = ("hello", "req", "res", "bye", "ack")
MAX_LINE_BYTES = 4000  # 与 src/protocol/mod.rs 的 MAX_LINE_BYTES 一致
# agent-state 的字节上限，与 src/agent/record.rs 一致（title/msg 来自 OSC 7501 rev 0.4）。
AGENT_BYTE_LIMITS = {"title": 192, "msg": 2048, "origin_session": 128, "scope.id": 4096}
BUILTIN_CONFIG = os.path.join(ROOT, "src", "config", "builtin.json")
PRESET_DIR = os.path.join(ROOT, "src", "config", "presets")
PALETTE_DIR = os.path.join(ROOT, "src", "config", "palettes")
# 与 src/config/palette.rs 的 ROLES 一致。
ROLES = ("bg1", "bg2", "bg3", "bg4", "bg5", "bg6", "fg", "fg_dim", "surface", "accent", "ok", "warn", "err", "info")
HEX_RE = re.compile(r"^#[0-9a-fA-F]{6}$")

try:
    from jsonschema import Draft202012Validator
    from jsonschema.exceptions import best_match
except ImportError:
    print("jsonschema 未安装；本脚本不自动安装依赖，请自行安装后重试（pip install jsonschema）。", file=sys.stderr)
    sys.exit(2)


def builtin_module_ids():
    try:
        with open(BUILTIN_CONFIG, encoding="utf-8") as f:
            return set((json.load(f).get("modules") or {}).keys())
    except (OSError, json.JSONDecodeError):
        return set()


def builtin_palettes():
    try:
        return {fn[: -len(".json")] for fn in os.listdir(PALETTE_DIR) if fn.endswith(".json")}
    except OSError:
        return set()


def extra_checks(schema_name, doc):
    """JSON Schema 无法表达的检查（跨引用、按字节计数）。"""
    errors = []
    if schema_name == "config.v1" and isinstance(doc, dict) and isinstance(doc.get("palette"), str):
        known = builtin_palettes() | set((doc.get("palettes") or {}).keys())
        if doc["palette"] not in known:
            errors.append(f"palette '{doc['palette']}' 既不是内置调色板，也没有在 palettes 中定义")
    if schema_name in ("config.v0", "config.v1") and isinstance(doc, dict):
        module_ids = set((doc.get("modules") or {}).keys())
        if schema_name == "config.v1":
            module_ids |= builtin_module_ids()
        rows = (doc.get("layout") or {}).get("rows") or []
        for i, row in enumerate(rows):
            for side in ("left", "right"):
                for ref in row.get(side) or []:
                    if ref not in module_ids:
                        errors.append(f"layout.rows[{i}].{side} 引用了未定义的模块 '{ref}'")
    if schema_name == "agent-state" and isinstance(doc, dict):
        # OSC 7501 counts title/msg in UTF-8 bytes; JSON Schema maxLength counts characters.
        for key, limit in AGENT_BYTE_LIMITS.items():
            value = doc.get(key) if key != "scope.id" else (doc.get("scope") or {}).get("id")
            if isinstance(value, str) and len(value.encode("utf-8")) > limit:
                errors.append(f"{key}: {len(value.encode('utf-8'))} 字节，超过 {limit} 字节上限（OSC 7501）")
    if schema_name == SHELL_SCHEMA and isinstance(doc, dict) and doc.get("frame") == "RES":
        lines = list(doc.get("left") or []) + [line for k in ("right", "transient") if k in doc for line in doc[k].split("\n")]
        history = doc.get("history") or {}
        lines += [line for k in ("context_transient", "context_suffix") if history.get(k) for line in history[k].split("\n")]
        if history.get("context_transient") and len(history["context_transient"].split("\n")) > len(doc.get("left") or []):
            errors.append("context transient exceeds original prompt height")
        for i, text in enumerate(lines):
            size = len(text.encode("utf-8"))
            if size > MAX_LINE_BYTES:
                errors.append(f"RES 第 {i + 1} 个提示符行 {size} 字节，超过 {MAX_LINE_BYTES} 字节上限")
    return errors


def load_schemas():
    schemas = {}
    for fn in sorted(os.listdir(SCHEMA_DIR)):
        if not fn.endswith(SCHEMA_SUFFIX):
            continue
        with open(os.path.join(SCHEMA_DIR, fn), encoding="utf-8") as f:
            schema = json.load(f)
        Draft202012Validator.check_schema(schema)
        schemas[fn[: -len(SCHEMA_SUFFIX)]] = Draft202012Validator(schema)
    return schemas


def classify(path):
    """返回 (schema, kind)。schema 为 None 表示忽略（子目录中的数据文件）；为 '?' 表示顶层命名不合规。"""
    rel_dir, fn = os.path.split(os.path.relpath(path, FIXTURE_DIR))
    if rel_dir in DIR_SCHEMA:
        if fn.startswith("valid"):
            return DIR_SCHEMA[rel_dir], "valid"
        if fn.startswith("invalid"):
            return DIR_SCHEMA[rel_dir], "invalid"
        return "?", None
    m = NAME_RE.match(fn)
    if m:
        return m.group("schema"), m.group("kind")
    return ("?" if rel_dir == "" else None), None


def frame_of(schema, kind, fn, doc):
    """shell-frame.v1 样本的帧类型；其他 schema 返回 None。"""
    if schema != SHELL_SCHEMA:
        return None
    if kind == "valid":
        frame = str(doc.get("frame", "")).lower() if isinstance(doc, dict) else ""
        return frame if frame in FRAMES else None
    m = re.search(r"\.invalid-([a-z]+)", fn)
    return m.group(1) if m and m.group(1) in FRAMES else None


def unescape(s):
    """反转义线路字段：\\\\ -> \\，\\t -> TAB，\\n -> LF；其他反斜杠序列视为错误。"""
    out, i = [], 0
    while i < len(s):
        c = s[i]
        if c == "\\":
            nxt = s[i + 1] if i + 1 < len(s) else ""
            mapping = {"\\": "\\", "t": "\t", "n": "\n"}
            if nxt not in mapping:
                raise ValueError(f"非法转义 \\{nxt!r}")
            out.append(mapping[nxt])
            i += 2
        else:
            out.append(c)
            i += 1
    return "".join(out)


def decode_frame(lines):
    """把一帧的行列表解码为 shell-frame.v1 逻辑对象。"""
    head = lines[0].split("\t")
    tag = head[0]
    if tag == "HELLO":
        _, pv, bv, ep, pid = head
        return {"frame": "HELLO", "protocol_version": int(pv), "binary_version": bv,
                "worker_epoch": int(ep), "pid": int(pid)}
    if tag == "REQ":
        _, gen, sid, cwd, cols, lns, last_exit, dur, env = head
        return {"frame": "REQ", "gen": int(gen), "session_id": sid, "cwd": unescape(cwd),
                "cols": int(cols), "lines": int(lns), "last_exit": int(last_exit),
                "last_duration_ms": int(dur), "env": unescape(env)}
    if tag == "ACK":
        _, gen, epoch, seq = head
        return {"frame":"ACK", "gen":int(gen), "epoch":int(epoch), "seq":int(seq)}
    if tag == "RES":
        _, gen, status, ep = head
        obj = {"frame": "RES", "gen": int(gen), "status": status, "worker_epoch": int(ep), "left": []}
        end_gen = None
        for line in lines[1:]:
            kind, _, content = line.partition("\t")
            if kind == "L":
                obj["left"].append(content)
            elif kind == "R":
                obj["right"] = content
            elif kind == "T":
                obj["transient"] = (obj["transient"] + "\n" + content) if "transient" in obj else content
            elif kind in ("O", "K", "U", "S", "H"):
                if kind == "O":
                    if "observation_seq" in obj: raise ValueError("duplicate observation sequence")
                    obj["observation_seq"] = int(content)
                else:
                    history = obj.setdefault("history", {"context_transient":None, "context_suffix":None, "summary":False, "summary_threshold_ms":3000})
                    key = {"K":"context_key", "H":"context_transient", "U":"context_suffix", "S":"summary_threshold_ms"}[kind]
                    if kind == "H": history[key] = (history[key] + "\n" + content) if history[key] is not None else content
                    elif kind == "S": history["summary"] = True; history[key] = int(content)
                    else: history[key] = content
            elif kind == "END":
                end_gen = int(content)
            else:
                raise ValueError(f"RES 帧中出现未知行 {kind!r}")
        if end_gen is None:
            raise ValueError("RES 帧缺少 END 行")
        if end_gen != obj["gen"]:
            raise ValueError(f"END gen {end_gen} 与 RES gen {obj['gen']} 不一致")
        return obj
    if tag == "BYE":
        if len(lines) != 1 or len(head) != 1:
            raise ValueError("BYE 必须单独一行且无其他字段")
        return {"frame": "BYE"}
    raise ValueError(f"未知帧标签 {tag!r}")


def split_frames(text):
    frames, cur = [], []
    for line in text.split("\n"):
        if line == "":
            if cur:
                frames.append(cur)
                cur = []
        else:
            cur.append(line)
    if cur:
        frames.append(cur)
    return frames


def main():
    schemas = load_schemas()
    failures = 0
    coverage = defaultdict(int)  # (schema, kind) -> count
    frame_cov = defaultdict(int)  # (frame, kind) -> count, shell-frame.v1 JSON samples
    wire_cov = defaultdict(int)  # frame -> count, wire samples

    print(f"validator: jsonschema {importlib.metadata.version('jsonschema')} / Draft202012Validator")
    print(f"schemas: {', '.join(sorted(schemas))}")
    print("\n== JSON 样本")
    for dirpath, dirnames, filenames in os.walk(FIXTURE_DIR):
        dirnames[:] = sorted(d for d in dirnames if not d.startswith("."))
        for fn in sorted(filenames):
            if not fn.endswith(".json"):
                continue
            path = os.path.join(dirpath, fn)
            label = os.path.relpath(path, FIXTURE_DIR)
            schema, kind = classify(path)
            if schema is None:
                continue  # 子目录中的其他数据文件
            if schema == "?":
                print(f"FAIL  {label}: 文件名不符合 <schema>.valid|invalid[-后缀].json")
                failures += 1
                continue
            if schema not in schemas:
                print(f"FAIL  {label}: 找不到 schema/{schema}{SCHEMA_SUFFIX}")
                failures += 1
                continue
            try:
                with open(path, encoding="utf-8") as f:
                    doc = json.load(f)
            except (OSError, json.JSONDecodeError) as exc:
                print(f"FAIL  {label}: 无法解析为 JSON: {exc}")
                failures += 1
                continue

            best = best_match(schemas[schema].iter_errors(doc))
            problems = []
            if best is not None:
                problems.append(f"{'/'.join(str(p) for p in best.absolute_path) or '<root>'}: {best.message}")
            problems += extra_checks(schema, doc)

            coverage[(schema, kind)] += 1
            frame = frame_of(schema, kind, fn, doc)
            if frame:
                frame_cov[(frame, kind)] += 1
            if kind == "valid":
                ok = not problems
                detail = "" if ok else "; ".join(problems)
            else:
                ok = bool(problems)
                detail = "按预期失败: " + ("; ".join(problems) if problems else "（实际通过，不符合预期）")
            failures += 0 if ok else 1
            print(f"{'PASS' if ok else 'FAIL'}  {label}  [{kind}, schema={schema}]  {detail}".rstrip())

    print("\n== 内置配置与预设 src/config/{builtin.json,presets/*.json}（config.v1）")
    builtin_count = 0
    for path in [BUILTIN_CONFIG] + [os.path.join(PRESET_DIR, fn) for fn in sorted(os.listdir(PRESET_DIR)) if fn.endswith(".json")]:
        label = os.path.relpath(path, ROOT)
        try:
            with open(path, encoding="utf-8") as f:
                doc = json.load(f)
        except (OSError, json.JSONDecodeError) as exc:
            print(f"FAIL  {label}: 无法解析为 JSON: {exc}")
            failures += 1
            continue
        if path != BUILTIN_CONFIG and isinstance(doc, dict):
            doc = {"schema_version": 1, **doc}  # 预设是分层文件，省略 schema_version
        best = best_match(schemas["config.v1"].iter_errors(doc))
        problems = []
        if best is not None:
            problems.append(f"{'/'.join(str(p) for p in best.absolute_path) or '<root>'}: {best.message}")
        problems += extra_checks("config.v1", doc)
        ok = not problems
        failures += 0 if ok else 1
        builtin_count += 1
        print(f"{'PASS' if ok else 'FAIL'}  {label}  {'; '.join(problems)}".rstrip())

    print("\n== 内置调色板 src/config/palettes/*.json（config.v1 $defs/palette + 角色色）")
    palette_schema = Draft202012Validator({"$ref": "#/$defs/palette", "$defs": schemas["config.v1"].schema["$defs"]})
    for fn in sorted(os.listdir(PALETTE_DIR)):
        if not fn.endswith(".json"):
            continue
        path = os.path.join(PALETTE_DIR, fn)
        label = os.path.relpath(path, ROOT)
        try:
            with open(path, encoding="utf-8") as f:
                doc = json.load(f)
        except (OSError, json.JSONDecodeError) as exc:
            print(f"FAIL  {label}: 无法解析为 JSON: {exc}")
            failures += 1
            continue
        problems = []
        best = best_match(palette_schema.iter_errors(doc))
        if best is not None:
            problems.append(f"{'/'.join(str(p) for p in best.absolute_path) or '<root>'}: {best.message}")
        missing = [r for r in ROLES + ("background", "foreground") if r not in doc]
        if missing:
            problems.append(f"缺少 {', '.join(missing)}")
        not_hex = [k for k, v in doc.items() if k != "$comment" and not HEX_RE.match(str(v))]
        if not_hex:
            problems.append(f"值不是 #rrggbb: {', '.join(not_hex)}")
        ok = not problems
        failures += 0 if ok else 1
        builtin_count += 1
        print(f"{'PASS' if ok else 'FAIL'}  {label}  {'; '.join(problems)}".rstrip())

    print("\n== 线路样本 wire-samples.txt")
    with open(WIRE, encoding="utf-8", newline="") as f:
        frames = split_frames(f.read())
    for i, lines in enumerate(frames, 1):
        label = f"frame#{i} {lines[0].split(chr(9))[0]}"
        try:
            obj = decode_frame(lines)
        except (ValueError, IndexError) as exc:
            print(f"FAIL  {label}: 解码失败: {exc}")
            failures += 1
            continue
        problems = []
        best = best_match(schemas[SHELL_SCHEMA].iter_errors(obj))
        if best is not None:
            problems.append(f"{'/'.join(str(p) for p in best.absolute_path) or '<root>'}: {best.message}")
        problems += extra_checks(SHELL_SCHEMA, obj)
        if obj["frame"] == "REQ" and not ("\t" in obj["cwd"] and "\\" in obj["cwd"]):
            problems.append("REQ cwd 反转义后应同时含制表符与反斜杠")
        ok = not problems
        failures += 0 if ok else 1
        print(f"{'PASS' if ok else 'FAIL'}  {label}  {'; '.join(problems)}".rstrip())
        if ok:
            wire_cov[obj["frame"].lower()] += 1

    print("\n== 覆盖")
    for name in sorted(schemas):
        for kind in ("valid", "invalid"):
            if coverage[(name, kind)] == 0:
                print(f"FAIL  覆盖不足: {name} 缺少 {kind} 样本")
                failures += 1
    for fr in FRAMES:
        for kind in ("valid", "invalid"):
            if frame_cov[(fr, kind)] == 0:
                print(f"FAIL  shell-frame.v1 缺少 {fr} 帧的 {kind} 样本")
                failures += 1
        if wire_cov[fr] == 0:
            print(f"FAIL  wire-samples.txt 缺少 {fr} 帧")
            failures += 1
    summary = ", ".join(f"{n} {k}={coverage[(n, k)]}" for n in sorted(schemas) for k in ("valid", "invalid"))
    print(f"schema 样本数: {summary}")
    print(f"shell-frame.v1 帧样本数(JSON valid/invalid): " + ", ".join(
        f"{fr} {frame_cov[(fr, 'valid')]}/{frame_cov[(fr, 'invalid')]}" for fr in FRAMES))
    print(f"线路样本帧数: " + ", ".join(f"{fr} {wire_cov[fr]}" for fr in FRAMES))

    total = sum(coverage.values()) + len(frames) + builtin_count
    print(f"\n结果: {total} 个样本，{'全部通过' if failures == 0 else f'{failures} 项失败'}")
    return 0 if failures == 0 else 1


if __name__ == "__main__":
    sys.exit(main())
