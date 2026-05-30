#!/usr/bin/env python3
"""
Lambda Agent Sandbox — Comprehensive Benchmark & Test Suite
─────────────────────────────────────────────────────────────
Runs 50+ test cases across all runtimes (bash, python, node),
validates output, measures performance, and generates a
professional markdown report.

Usage:
    python3 scripts/benchmark_sandbox.py \\
        --function arn:aws:lambda:eu-central-1:403012596812:function:test-lambda-sandbox \\
        --region eu-central-1 \\
        --profile default \\
        --warmup 3 \\
        --output report.md \\
        --json-output results.json

    # With Function URL instead (no AWS CLI needed):
    python3 scripts/benchmark_sandbox.py \\
        --url https://xxx.lambda-url.eu-central-1.on.aws/ \\
        --output report.md \\
        --json-output results.json

    # With .env file for config (create ./.env):
    #   FUNCTION_ARN=arn:aws:lambda:...
    #   REGION=eu-central-1
    #   PROFILE=default
    #   WARMUP=3
    #   BENCHMARK_RUNS=5
    python3 scripts/benchmark_sandbox.py -e .env -o report.md -j results.json

Requirements:
    - Python 3.8+
    - aws CLI installed & configured (for --function mode)
    - requests library (for --url mode): pip install requests
"""

import json
import subprocess
import sys
import time
import argparse
import os
from datetime import datetime, timezone
from typing import Optional
from statistics import median, stdev


# ═══════════════════════════════════════════════════════════════════════════════
#  .env file loader
# ═══════════════════════════════════════════════════════════════════════════════

def _load_env_file(path: str) -> dict:
    """Parse a .env file and return a dict of key→value.

    Supports:
    - KEY=VALUE or KEY="VALUE" or KEY='VALUE'
    - Comments with # (full-line or inline)
    - Blank lines
    - No shell-style variable expansion ($VAR)
    """
    env = {}
    try:
        with open(path) as f:
            for line in f:
                raw = line.strip()
                # Strip inline comment (respecting quoted values)
                in_quote = None
                comment_pos = -1
                for i, ch in enumerate(raw):
                    if ch in ("'", '"') and (in_quote is None):
                        in_quote = ch
                    elif ch == in_quote:
                        in_quote = None
                    elif ch == '#' and in_quote is None:
                        comment_pos = i
                        break
                if comment_pos >= 0:
                    raw = raw[:comment_pos].strip()
                if not raw or raw.startswith('#'):
                    continue
                if '=' not in raw:
                    continue
                key, _, val = raw.partition('=')
                key = key.strip()
                val = val.strip()
                # Strip surrounding quotes
                if len(val) >= 2 and val[0] == val[-1] and val[0] in ("'", '"'):
                    val = val[1:-1]
                if key:
                    env[key] = val
    except FileNotFoundError:
        pass  # silently ignore missing .env files
    return env


def _apply_env_to_args(args, env: dict) -> None:
    """Merge .env values into parsed args — CLI args take precedence."""
    # Only set if the CLI value is the default (i.e., not explicitly provided)
    if not args.function and not args.url:
        fn = env.get("FUNCTION_ARN") or env.get("LAMBDA_FUNCTION_ARN")
        url = env.get("URL") or env.get("LAMBDA_URL")
        if fn:
            args.function = fn
        elif url:
            args.url = url
    if args.region == "eu-central-1":
        args.region = env.get("REGION") or env.get("AWS_REGION") or args.region
    if args.profile == "default":
        args.profile = env.get("PROFILE") or env.get("AWS_PROFILE") or args.profile
    if args.warmup == 2:
        args.warmup = int(env.get("WARMUP", args.warmup))
    if args.benchmark_runs == 5:
        args.benchmark_runs = int(env.get("BENCHMARK_RUNS", args.benchmark_runs))


# ═══════════════════════════════════════════════════════════════════════════════
#  Test registry – each test is a dict with:
#    name        : short identifier
#    description : human-readable label
#    payload     : dict sent as the Lambda event
#    check       : callable(response) → (bool, str)  (pass? , detail)
#    category    : grouping label
# ═══════════════════════════════════════════════════════════════════════════════

TESTS: list[dict] = []


def test(**kw):
    """Register a test case — validates required fields at definition time."""
    _REQUIRED = {"name", "description", "payload", "check", "category"}
    missing = _REQUIRED - set(kw.keys())
    if missing:
        raise ValueError(
            f"Test '{kw.get('name', 'unknown')}' missing required fields: "
            f"{', '.join(sorted(missing))}")
    kw.setdefault("skip", False)
    TESTS.append(kw)


def _safe_parse_int(s: str, default: int = 0) -> int:
    """Parse integer from a string, returning default on failure."""
    try:
        return int(s.strip())
    except (ValueError, TypeError):
        return default


# 🟢 ─── Section 1: Bash Fundamentals ──────────────────────────────────────────

test(
    name="bash_echo",
    description="Bash: basic echo",
    payload={"runtime": "bash", "code": "echo hello world", "timeout_ms": 30000},
    check=lambda r: (r.get("ok") is True and "hello world" in r.get("stdout", ""),
                     f"stdout={r.get('stdout','')!r}"),
    category="Bash Runtime",
)

test(
    name="bash_loop",
    description="Bash: for-loop with sequence",
    payload={"runtime": "bash", "code": "for i in 1 2 3; do echo line $i; done",
             "timeout_ms": 30000},
    check=lambda r: (r.get("ok") is True and "line 3" in r.get("stdout", ""),
                     f"stdout={r.get('stdout','')!r}"),
    category="Bash Runtime",
)

test(
    name="bash_pipe",
    description="Bash: pipe operator",
    payload={"runtime": "bash", "code": "echo pipe test | wc -w",
             "timeout_ms": 30000},
    check=lambda r: (r.get("ok") is True and "2" in r.get("stdout", ""),
                     f"stdout={r.get('stdout','')!r}"),
    category="Bash Runtime",
)

test(
    name="bash_subshell",
    description="Bash: subshell $(...) nesting",
    payload={"runtime": "bash", "code": "result=$(echo nested); echo $result",
             "timeout_ms": 30000},
    check=lambda r: (r.get("ok") is True and "nested" in r.get("stdout", ""),
                     f"stdout={r.get('stdout','')!r}"),
    category="Bash Runtime",
)

test(
    name="bash_args",
    description="Bash: positional args passthrough",
    payload={"runtime": "bash", "code": 'echo args=$#; for a in "$@"; do echo "  $a"; done',
             "args": ["hello", "world", "42"], "timeout_ms": 30000},
    check=lambda r: (r.get("ok") is True and "args=3" in r.get("stdout", "")
                     and "hello" in r.get("stdout", ""),
                     f"stdout={r.get('stdout','')!r}"),
    category="Bash Runtime",
)

test(
    name="bash_exit_code",
    description="Bash: non-zero exit code propagation",
    payload={"runtime": "bash", "code": "exit 42", "timeout_ms": 30000},
    check=lambda r: (r.get("ok") is False and r.get("exit_code") == 42,
                     f"exit_code={r.get('exit_code')}, ok={r.get('ok')}"),
    category="Bash Runtime",
)

test(
    name="bash_default_runtime",
    description="Bash: default runtime (no runtime field)",
    payload={"code": "echo default", "timeout_ms": 30000},
    check=lambda r: (r.get("ok") is True and r.get("runtime") == "bash",
                     f"runtime={r.get('runtime')!r}"),
    category="Bash Runtime",
)

# 🟢 ─── Section 2: Python Runtime ─────────────────────────────────────────────

test(
    name="python_basic",
    description="Python: basic print",
    payload={"runtime": "python", "code": "print('hello from python')",
             "timeout_ms": 30000},
    check=lambda r: (r.get("ok") is True and "hello from python" in r.get("stdout", ""),
                     f"stdout={r.get('stdout','')!r}"),
    category="Python Runtime",
)

test(
    name="python_json",
    description="Python: JSON generation",
    payload={"runtime": "python",
             "code": "import json; print(json.dumps({'items': [x*2 for x in range(10)]}))",
             "timeout_ms": 30000},
    check=lambda r: (r.get("ok") is True and "items" in r.get("stdout", ""),
                     f"stdout={r.get('stdout','')!r}"),
    category="Python Runtime",
)

test(
    name="python_subprocess",
    description="Python: subprocess execution",
    payload={"runtime": "python",
             "code": "import subprocess; r = subprocess.run(['echo','subproc'], capture_output=True, text=True); print(r.stdout.strip())",
             "timeout_ms": 30000},
    check=lambda r: (r.get("ok") is True and "subproc" in r.get("stdout", ""),
                     f"stdout={r.get('stdout','')!r}"),
    category="Python Runtime",
)

test(
    name="python_math",
    description="Python: math module",
    payload={"runtime": "python",
             "code": "import math; print(f'pi={math.pi:.5f}, e={math.e:.5f}')",
             "timeout_ms": 30000},
    check=lambda r: (r.get("ok") is True and "pi=3.14159" in r.get("stdout", ""),
                     f"stdout={r.get('stdout','')!r}"),
    category="Python Runtime",
)

test(
    name="python_error",
    description="Python: runtime error handling",
    payload={"runtime": "python", "code": "x = 1/0", "timeout_ms": 30000},
    check=lambda r: (r.get("ok") is False and r.get("exit_code") == 1
                     and "ZeroDivisionError" in r.get("stderr", ""),
                     f"exit_code={r.get('exit_code')}, stderr={r.get('stderr','')!r}"),
    category="Python Runtime",
)

# 🟢 ─── Section 3: Node.js Runtime ────────────────────────────────────────────

test(
    name="node_basic",
    description="Node.js: basic console.log",
    payload={"runtime": "node", "code": "console.log('hello from node');",
             "timeout_ms": 30000},
    check=lambda r: (r.get("ok") is True and "hello from node" in r.get("stdout", ""),
                     f"stdout={r.get('stdout','')!r}"),
    category="Node.js Runtime",
)

test(
    name="node_async",
    description="Node.js: setTimeout async execution",
    payload={"runtime": "node",
             "code": "setTimeout(() => { console.log('async done'); }, 100);",
             "timeout_ms": 30000},
    check=lambda r: (r.get("ok") is True and "async done" in r.get("stdout", ""),
                     f"stdout={r.get('stdout','')!r}"),
    category="Node.js Runtime",
)

test(
    name="node_promise",
    description="Node.js: Promise resolution",
    payload={"runtime": "node",
             "code": "Promise.resolve('promise works').then(console.log);",
             "timeout_ms": 30000},
    check=lambda r: (r.get("ok") is True and "promise works" in r.get("stdout", ""),
                     f"stdout={r.get('stdout','')!r}"),
    category="Node.js Runtime",
)

test(
    name="node_catch",
    description="Node.js: try/catch error handling",
    payload={"runtime": "node",
             "code": "try { throw new Error('boom'); } catch(e) { console.log('caught:', e.message); }",
             "timeout_ms": 30000},
    check=lambda r: (r.get("ok") is True and "caught" in r.get("stdout", ""),
                     f"stdout={r.get('stdout','')!r}"),
    category="Node.js Runtime",
)

test(
    name="node_fs",
    description="Node.js: filesystem read/write",
    payload={"runtime": "node",
             "code": "const fs=require('fs'); fs.writeFileSync('t.txt','hi'); console.log(fs.readFileSync('t.txt','utf8'));",
             "timeout_ms": 30000},
    check=lambda r: (r.get("ok") is True and "hi" in r.get("stdout", ""),
                     f"stdout={r.get('stdout','')!r}"),
    category="Node.js Runtime",
)

test(
    name="node_subprocess",
    description="Node.js: child_process subprocess",
    payload={"runtime": "node",
             "code": "const {execSync}=require('child_process'); console.log(execSync('echo subproc').toString().trim());",
             "timeout_ms": 30000},
    check=lambda r: (r.get("ok") is True and "subproc" in r.get("stdout", ""),
                     f"stdout={r.get('stdout','')!r}"),
    category="Node.js Runtime",
)

# 🟢 ─── Section 4: Runtime Aliases ────────────────────────────────────────────

for alias, rt, expected in [
    ("alias_sh", "sh", "sh alias"),
    ("alias_python3", "python3", "python3 alias"),
    ("alias_py", "py", "py alias"),
    ("alias_nodejs", "nodejs", "nodejs alias"),
    ("alias_js", "js", "js alias"),
    ("alias_javascript", "javascript", "javascript alias"),
]:
    test(
        name=alias,
        description=f"Runtime alias: '{rt}'",
        payload={"runtime": rt, "code": f"print('{expected}')" if 'py' in rt
                 else f"console.log('{expected}')" if 'node' in rt or rt in ('js', 'javascript')
                 else f"echo {expected}",
                 "timeout_ms": 30000},
        check=lambda r, exp=expected: (r.get("ok") is True and exp in r.get("stdout", ""),
                                        f"stdout={r.get('stdout','')!r}"),
        category="Runtime Aliases",
    )

# 🟢 ─── Section 5: Edge Cases ─────────────────────────────────────────────────

test(
    name="unicode_bash",
    description="Unicode: emoji & multi-byte chars (bash)",
    payload={"runtime": "bash", "code": "echo émojï 🚀 世界 🌍",
             "timeout_ms": 30000},
    check=lambda r: (r.get("ok") is True and "🚀" in r.get("stdout", "")
                     and "世界" in r.get("stdout", ""),
                     f"stdout={r.get('stdout','')!r}"),
    category="Edge Cases",
)

test(
    name="unicode_python",
    description="Unicode: multi-language with emoji (Python)",
    payload={"runtime": "python",
             "code": "print('🎉 ✅ 🚀'); print('你好世界'); print('السلام عليكم')",
             "timeout_ms": 30000},
    check=lambda r: (r.get("ok") is True and "🎉" in r.get("stdout", ""),
                     f"stdout={r.get('stdout','')!r}"),
    category="Edge Cases",
)

test(
    name="unicode_node",
    description="Unicode: Japanese with emoji (Node.js)",
    payload={"runtime": "node",
             "code": "console.log('🎉 ✅ 🚀'); console.log('こんにちは');",
             "timeout_ms": 30000},
    check=lambda r: (r.get("ok") is True and "🎉" in r.get("stdout", ""),
                     f"stdout={r.get('stdout','')!r}"),
    category="Edge Cases",
)

test(
    name="empty_code",
    description="Empty code string",
    payload={"runtime": "bash", "code": "", "timeout_ms": 30000},
    check=lambda r: (r.get("ok") is True and r.get("stdout", "") == "",
                     f"ok={r.get('ok')}, stdout={r.get('stdout','')!r}"),
    category="Edge Cases",
)

test(
    name="file_io",
    description="File I/O within workspace",
    payload={"runtime": "bash",
             "code": "echo content >f.txt; cat f.txt; rm f.txt; echo cleaned",
             "timeout_ms": 30000},
    check=lambda r: (r.get("ok") is True and "content" in r.get("stdout", "")
                     and "cleaned" in r.get("stdout", ""),
                     f"stdout={r.get('stdout','')!r}"),
    category="Edge Cases",
)

test(
    name="binary_output",
    description="Binary output via base64",
    payload={"runtime": "bash",
             "code": "dd if=/dev/urandom bs=512 count=2 2>/dev/null | base64 | wc -c",
             "timeout_ms": 30000},
    check=lambda r: (
        r.get("ok") is True
        and len(r.get("stdout", "").strip()) > 0
        and _safe_parse_int(r.get("stdout", "").strip(), 0) > 100,
        f"base64 length={r.get('stdout','')!r}"),
    category="Edge Cases",
)

# 🟢 ─── Section 6: Error Handling ─────────────────────────────────────────────

test(
    name="err_missing_code",
    description="Missing required 'code' field",
    payload={"runtime": "bash"},
    check=lambda r: (r.get("ok") is False and "missing field" in r.get("stderr", "")
                     and "code" in r.get("stderr", ""),
                     f"stderr={r.get('stderr','')!r}"),
    category="Error Handling",
)

test(
    name="err_invalid_runtime",
    description="Unsupported runtime",
    payload={"runtime": "ruby", "code": "puts 'hi'", "timeout_ms": 30000},
    check=lambda r: (r.get("ok") is False and "unsupported runtime" in r.get("stderr", ""),
                     f"stderr={r.get('stderr','')!r}"),
    category="Error Handling",
)

test(
    name="err_runtime_python",
    description="Python runtime error (ZeroDivisionError)",
    payload={"runtime": "python", "code": "x = 1/0", "timeout_ms": 30000},
    check=lambda r: (r.get("ok") is False and r.get("exit_code") == 1
                     and "ZeroDivisionError" in r.get("stderr", ""),
                     f"exit_code={r.get('exit_code')}"),
    category="Error Handling",
)

test(
    name="err_runtime_node",
    description="Node.js runtime error (throw)",
    payload={"runtime": "node", "code": "throw new Error('test-err');",
             "timeout_ms": 30000},
    check=lambda r: (r.get("ok") is False and r.get("exit_code", 0) != 0
                     and "test-err" in r.get("stderr", ""),
                     f"exit_code={r.get('exit_code')}, stderr={r.get('stderr','')!r}"),
    category="Error Handling",
)

# 🟢 ─── Section 7: Timeout Enforcement ────────────────────────────────────────

test(
    name="timeout_basic",
    description="Timeout: 500ms on a 10s sleep",
    payload={"runtime": "bash", "code": "sleep 10; echo done",
             "timeout_ms": 500},
    check=lambda r: (r.get("timed_out") is True
                     and r.get("duration_ms", 0) < 5000
                     and "timed out" in r.get("stderr", "").lower(),
                     f"timed_out={r.get('timed_out')}, duration_ms={r.get('duration_ms')}"),
    category="Timeout Enforcement",
)

test(
    name="timeout_longer",
    description="Timeout: 3s on a 10s sleep",
    payload={"runtime": "bash", "code": "sleep 10; echo done",
             "timeout_ms": 3000},
    check=lambda r: (r.get("timed_out") is True
                     and r.get("duration_ms", 0) < 7000,
                     f"timed_out={r.get('timed_out')}, duration_ms={r.get('duration_ms')}"),
    category="Timeout Enforcement",
)

# 🟢 ─── Section 8: Security & Isolation ───────────────────────────────────────

test(
    name="sec_user",
    description="Runs as unprivileged user",
    payload={"runtime": "bash", "code": "whoami; id -u; id -g",
             "timeout_ms": 30000},
    check=lambda r: (r.get("ok") is True
                     and "sbx_user" in r.get("stdout", "")
                     and "root" not in r.get("stdout", ""),
                     f"stdout={r.get('stdout','')!r}"),
    category="Security & Isolation",
)

test(
    name="sec_shadow",
    description="/etc/shadow access denied",
    payload={"runtime": "bash", "code": "cat /etc/shadow 2>&1; echo done",
             "timeout_ms": 30000},
    check=lambda r: (r.get("ok") is True
                     and "Permission denied" in r.get("stdout", "")
                     and "done" in r.get("stdout", ""),
                     f"stdout={r.get('stdout','')!r}"),
    category="Security & Isolation",
)

test(
    name="sec_workspace",
    description="Unique UUID workspace per invocation",
    payload={"runtime": "bash", "code": "pwd", "timeout_ms": 30000},
    check=lambda r: (r.get("ok") is True
                     and "/tmp/agent-workspace/" in r.get("stdout", ""),
                     f"pwd={r.get('stdout','')!r}, workspace={r.get('workspace','')!r}"),
    category="Security & Isolation",
)

test(
    name="sec_caps",
    description="Zero capabilities (fully unprivileged)",
    payload={"runtime": "bash",
             "code": "cat /proc/self/status 2>/dev/null | grep -E '^Cap' || echo no-proc",
             "timeout_ms": 30000},
    check=lambda r: (r.get("ok") is True
                     and ("CapEff:\t0000000000000000" in r.get("stdout", "")
                          or "CapEff" not in r.get("stdout", "")),
                     f"stdout={r.get('stdout','')!r}"),
    category="Security & Isolation",
)

test(
    name="sec_env_isolated",
    description="HOME/TMPDIR point to workspace",
    payload={"runtime": "bash", "code": "echo HOME=$HOME; echo TMPDIR=$TMPDIR",
             "timeout_ms": 30000},
    check=lambda r: (r.get("ok") is True
                     and "/tmp/agent-workspace/" in r.get("stdout", ""),
                     f"stdout={r.get('stdout','')!r}"),
    category="Security & Isolation",
)

# 🟢 ─── Section 9: Stress & Performance ───────────────────────────────────────

test(
    name="stress_heavy_output",
    description="Stress: 1,000 lines of output",
    payload={"runtime": "bash",
             "code": "for i in $(seq 1 1000); do echo 'line '$i; done",
             "timeout_ms": 30000},
    check=lambda r: (r.get("ok") is True
                     and len(r.get("stdout", "").split('\n')) >= 1000,
                     f"lines={len(r.get('stdout','').split(chr(10)))}"),
    category="Stress & Performance",
)

test(
    name="stress_cpu_bash",
    description="Stress: 100k no-op loop (bash)",
    payload={"runtime": "bash",
             "code": "for i in $(seq 1 100000); do : ; done; echo done",
             "timeout_ms": 30000},
    check=lambda r: (r.get("ok") is True and "done" in r.get("stdout", ""),
                     f"duration_ms={r.get('duration_ms')}"),
    category="Stress & Performance",
)

test(
    name="stress_cpu_python",
    description="Stress: 1M square sum (Python)",
    payload={"runtime": "python",
             "code": "print(f'sum={sum(i*i for i in range(1_000_000))}')",
             "timeout_ms": 30000},
    check=lambda r: (r.get("ok") is True
                     and "sum=" in r.get("stdout", ""),
                     f"stdout={r.get('stdout','')!r}"),
    category="Stress & Performance",
)

test(
    name="stress_primes",
    description="Stress: find 2262 primes up to 20000 (Python)",
    payload={"runtime": "python",
             "code": "primes=[]\nfor n in range(2,20000):\n    for p in primes:\n        if n%p==0: break\n    else: primes.append(n)\nprint(f'found {len(primes)} primes')",
             "timeout_ms": 60000},
    check=lambda r: (r.get("ok") is True and "2262" in r.get("stdout", ""),
                     f"stdout={r.get('stdout','')!r}"),
    category="Stress & Performance",
)

test(
    name="stress_memory",
    description="Stress: allocate 1M item list (Python)",
    payload={"runtime": "python",
             "code": "data=[x for x in range(1_000_000)]; print(f'allocated {len(data)} items')",
             "timeout_ms": 30000},
    check=lambda r: (r.get("ok") is True
                     and "allocated 1000000" in r.get("stdout", ""),
                     f"stdout={r.get('stdout','')!r}"),
    category="Stress & Performance",
)

test(
    name="stress_sleep5",
    description="Stress: 5-second sleep",
    payload={"runtime": "bash", "code": "echo before; sleep 5; echo after",
             "timeout_ms": 60000},
    check=lambda r: (r.get("ok") is True
                     and r.get("duration_ms", 0) >= 4500
                     and r.get("duration_ms", 0) <= 15000,
                     f"duration_ms={r.get('duration_ms')}"),
    category="Stress & Performance",
)

test(
    name="stress_sleep10",
    description="Stress: 10-second sleep",
    payload={"runtime": "bash", "code": "echo before; sleep 10; echo after",
             "timeout_ms": 60000},
    check=lambda r: (r.get("ok") is True
                     and r.get("duration_ms", 0) >= 9500,
                     f"duration_ms={r.get('duration_ms')}"),
    category="Stress & Performance",
)


# 🟢 ─── Section 10: Production-like Scripts ────────────────────────────────
# These tests simulate real-world workloads: log processing, data pipelines,
# file manipulation, configuration parsing, and API response generation.
# They exercise the sandbox with multi-step logic that mirrors production usage.
# ═══════════════════════════════════════════════════════════════════════════════

# ── Helper: named checks for production scripts ──────────────────────────────

def _check_stdout_contains(expected: str):
    """Factory: returns a check that ensures `expected` is found in stdout."""
    return lambda r: (
        r.get("ok") is True and expected in r.get("stdout", ""),
        f"stdout={r.get('stdout','')!r}")


def _check_stdout_all(expected_parts: list[str]):
    """Factory: returns a check that ensures ALL expected strings are in stdout."""
    def check(r):
        stdout = r.get("stdout", "")
        missing = [p for p in expected_parts if p not in stdout]
        if missing:
            return False, f"missing: {missing}, stdout={stdout!r}"
        return True, ""
    return check


# ── Bash: Production-like file manipulation ───────────────────────────────────

test(
    name="bash_log_parser",
    description="Bash: Apache access log parsing pipeline",
    category="Production-like Scripts",
    payload={"runtime": "bash", "code": "cat > access.log << 'LOGDATA'\n192.168.1.1 - - [10/Jan/2024:08:00:01 +0000] \"GET /api/users HTTP/1.1\" 200 1234\n192.168.1.2 - - [10/Jan/2024:08:00:05 +0000] \"GET /api/items HTTP/1.1\" 404 56\n192.168.1.1 - - [10/Jan/2024:08:00:10 +0000] \"POST /api/users HTTP/1.1\" 201 78\n192.168.1.3 - - [10/Jan/2024:08:00:15 +0000] \"GET /api/users HTTP/1.1\" 200 5678\n192.168.1.1 - - [10/Jan/2024:08:00:20 +0000] \"GET /api/users HTTP/1.1\" 500 12\n192.168.1.4 - - [10/Jan/2024:08:00:25 +0000] \"DELETE /api/sessions HTTP/1.1\" 204 0\n192.168.1.2 - - [10/Jan/2024:08:00:30 +0000] \"GET /api/items HTTP/1.1\" 304 0\n192.168.1.5 - - [10/Jan/2024:08:00:35 +0000] \"POST /api/auth HTTP/1.1\" 200 234\n192.168.1.1 - - [10/Jan/2024:08:00:40 +0000] \"GET /api/users HTTP/1.1\" 200 89\n192.168.1.3 - - [10/Jan/2024:08:00:45 +0000] \"PUT /api/users/42 HTTP/1.1\" 200 45\nLOGDATA\nTOTAL=$(wc -l < access.log)\nUNIQUE_IPS=$(awk '{print $1}' access.log | sort -u | wc -l)\nOK200=$(grep -c ' 200 ' access.log)\nCLIENT_ERR=$(grep -c ' 4[0-9][0-9] ' access.log)\nSERVER_ERR=$(grep -c ' 5[0-9][0-9] ' access.log)\nTOP_IP=$(awk '{print $1}' access.log | sort | uniq -c | sort -rn | head -1 | awk '{print $2}')\nTOP_IP_COUNT=$(awk '{print $1}' access.log | sort | uniq -c | sort -rn | head -1 | awk '{print $1}')\necho \"total=$TOTAL unique_ips=$UNIQUE_IPS ok=$OK200 client_errors=$CLIENT_ERR server_errors=$SERVER_ERR top_ip=$TOP_IP top_count=$TOP_IP_COUNT\"\nrm -f access.log",
             "timeout_ms": 30000},
    check=_check_stdout_all(["total=10", "unique_ips=5", "ok=5", "client_errors=2",
                            "server_errors=1", "top_ip=192.168.1.1", "top_count=4"]),
)

test(
    name="bash_file_batch_rename",
    description="Bash: batch file rename with sed substitution",
    category="Production-like Scripts",
    payload={"runtime": "bash", "code": """mkdir -p data
# Create report files with date pattern
for d in 20240101 20240102 20240103; do
  echo "data for $d" > "data/report_${d}.csv"
done
# Create backup files
for f in data/*.csv; do
  cp "$f" "${f}.bak"
done
# Rename .bak to .backup
for f in data/*.bak; do
  mv "$f" "${f%.bak}.backup"
done
# Count files by extension
echo "csv_count=$(ls data/*.csv 2>/dev/null | wc -l)"
echo "backup_count=$(ls data/*.backup 2>/dev/null | wc -l)"
echo "total_files=$(ls data/* 2>/dev/null | wc -l)"
# Verify content integrity
echo "content_check=$(cat data/report_20240101.csv)"
rm -rf data""",
             "timeout_ms": 30000},
    check=_check_stdout_all(["csv_count=3", "backup_count=3", "total_files=6", "data for 20240101"]),
)

test(
    name="bash_data_pipeline",
    description="Bash: CSV generation → sort → uniq → awk pipeline",
    category="Production-like Scripts",
    payload={"runtime": "bash", "code": """# Generate sales CSV: product,category,amount
cat > sales.csv << 'CSVDATA'
widget,gadgets,100
gizmo,gadgets,250
widget,gadgets,75
doodad,trinkets,30
gizmo,gadgets,300
widget,gadgets,150
doodad,trinkets,45
contraption,devices,500
gizmo,gadgets,200
doodad,trinkets,60
widget,gadgets,90
contraption,devices,350
CSVDATA
# Top product by sales volume
echo "top_product=$(awk -F',' '{a[$1]+=$3} END{for(p in a) print a[p],p}' sales.csv | sort -rn | head -1 | awk '{print $2}')"
echo "top_amount=$(awk -F',' '{a[$1]+=$3} END{for(p in a) print a[p],p}' sales.csv | sort -rn | head -1 | awk '{print $1}')"
# Category totals
echo "cat_summary=$(awk -F',' '{a[$2]+=$3} END{for(c in a) print c":"a[c]}' sales.csv | sort)"
# Transaction count
echo "tx_count=$(wc -l < sales.csv)"
# Average sale
echo "avg_sale=$(awk -F',' '{s+=$3} END{printf \"%.0f\", s/NR}' sales.csv)"
rm -f sales.csv""",
             "timeout_ms": 30000},            check=_check_stdout_all(["top_product=contraption", "top_amount=850",
                            "gadgets:1075", "trinkets:135", "devices:850",
                            "tx_count=12", "avg_sale=179"]),
)

test(
    name="bash_backup_restore",
    description="Bash: tar backup → restore to new dir → verify",
    category="Production-like Scripts",    payload={"runtime": "bash", "code": """mkdir -p project/src project/docs
# Create source files
echo 'def hello():\n    return "hello"' > project/src/main.py
echo 'import main\nprint(main.hello())' > project/src/app.py
echo '# Documentation' > project/docs/README.md
# Create backup
tar -czf backup.tar.gz project 2>/dev/null
# Restore to new location
mkdir -p restore
cd restore && tar -xzf ../backup.tar.gz 2>/dev/null
# Verify structure and content (we are now inside restore/)
echo "struct_ok=$([ -f project/src/main.py ] && [ -f project/src/app.py ] && [ -f project/docs/README.md ] && echo yes || echo no)"
echo "content_ok=$(diff ../project/src/main.py project/src/main.py 2>/dev/null && echo yes || echo no)"
echo "backup_size=$(wc -c < ../backup.tar.gz)"
# Cleanup
cd .. && rm -rf project backup.tar.gz restore""",
             "timeout_ms": 30000},
    check=_check_stdout_all(["struct_ok=yes", "content_ok=yes"]),
)

test(
    name="bash_csv_aggregation",
    description="Bash: CSV sales analysis with awk aggregation",
    category="Production-like Scripts",
    payload={"runtime": "bash", "code": """cat > orders.csv << 'CSV'
order_id,customer,amount,region,date
ORD001,Alice,150.00,NA,2024-01-15
ORD002,Bob,275.50,EU,2024-01-16
ORD003,Alice,89.99,NA,2024-02-01
ORD004,Charlie,450.00,APAC,2024-02-05
ORD005,Bob,32.50,EU,2024-02-10
ORD006,Alice,210.00,NA,2024-02-15
ORD007,Diana,675.00,NA,2024-03-01
ORD008,Charlie,120.00,APAC,2024-03-05
ORD009,Bob,500.00,EU,2024-03-10
ORD010,Diana,85.00,NA,2024-03-15
CSV
# Top customer by total spend
echo "top_cust=$(awk -F',' 'NR>1 {a[$2]+=$3} END{for(c in a) print a[c],c}' orders.csv | sort -rn | head -1 | awk '{print $2}')"
echo "top_spend=$(awk -F',' 'NR>1 {a[$2]+=$3} END{for(c in a) print a[c],c}' orders.csv | sort -rn | head -1 | awk '{print $1}')"
# Region totals
echo "regions=$(awk -F',' 'NR>1 {a[$4]+=$3} END{for(r in a) printf \"%s:%.0f \", r, a[r]}' orders.csv)"
# Order count
echo "order_count=$(( $(wc -l < orders.csv) - 1 ))"
# Total revenue
echo "total_rev=$(awk -F',' 'NR>1 {s+=$3} END{printf \"%.2f\", s}' orders.csv)"
# Average order value
echo "avg_order=$(awk -F',' 'NR>1 {s+=$3; n++} END{printf \"%.2f\", s/n}' orders.csv)"
rm -f orders.csv""",
             "timeout_ms": 30000},            check=_check_stdout_all(["top_cust=Bob", "top_spend=808.00",
                            "NA:", "EU:", "APAC:",
                            "order_count=10", "total_rev=2587.99"]),
)

# ── Python: Production-like data processing ───────────────────────────────────

test(
    name="python_csv_processor",
    description="Python: CSV parsing, filtering, and aggregation",
    category="Production-like Scripts",
    payload={"runtime": "python", "code": """import csv, io, json

CSV_DATA = \"\"\"name,role,salary,department
Alice,Engineer,95000,Engineering
Bob,Designer,82000,Design
Charlie,Engineer,105000,Engineering
Diana,Manager,115000,Engineering
Eve,Designer,78000,Design
Frank,Engineer,98000,Engineering
Grace,Intern,45000,Engineering
Henry,Manager,125000,Design
\"\"\"

reader = csv.DictReader(io.StringIO(CSV_DATA))
rows = list(reader)

# Department stats
dept_stats = {}
for r in rows:
    d = r['department']
    dept_stats.setdefault(d, {'count': 0, 'total': 0.0, 'roles': set()})
    dept_stats[d]['count'] += 1
    dept_stats[d]['total'] += float(r['salary'])
    dept_stats[d]['roles'].add(r['role'])

# Filter: employees earning > 90k
high_earners = [r for r in rows if float(r['salary']) > 90000]
high_earners.sort(key=lambda x: float(x['salary']), reverse=True)

result = {
    'employee_count': len(rows),
    'departments': list(dept_stats.keys()),
    'dept_avg_salary': {d: round(s['total']/s['count'], 2) for d, s in dept_stats.items()},
    'high_earners_count': len(high_earners),
    'top_earner': high_earners[0]['name'] if high_earners else None,
    'unique_roles': sorted(set(r['role'] for r in rows)),
}
print(json.dumps(result, indent=2))""",
             "timeout_ms": 30000},
    check=_check_stdout_all(['"employee_count": 8', '"departments": ["Engineering", "Design"]',
                            '"high_earners_count": 5', '"top_earner": "Henry"',
                            '"unique_roles": ["Designer", "Engineer", "Intern", "Manager"]']),
)

test(
    name="python_data_transform",
    description="Python: complex dict/list data transformation pipeline",
    category="Production-like Scripts",
    payload={"runtime": "python", "code": """import json

# Simulate API response processing
raw_data = [
    {'id': i, 'user': f'user_{i}', 'score': (i * 7) % 100,
     'tags': ['tag_' + str((i+j)%5) for j in range(3)],
     'active': i % 3 != 0}
    for i in range(50)
]

# Filter active records with score > 50
active_high = [d for d in raw_data if d['active'] and d['score'] > 50]

# Group by tag (flatten) and compute averages
tag_scores = {}
for d in raw_data:
    for t in d['tags']:
        tag_scores.setdefault(t, []).append(d['score'])

tag_avg = {t: round(sum(s)/len(s), 1) for t, s in sorted(tag_scores.items())}

# Compute percentile buckets
buckets = {'0-20': 0, '21-40': 0, '41-60': 0, '61-80': 0, '81-100': 0}
for d in raw_data:
    s = d['score']
    if s <= 20: buckets['0-20'] += 1
    elif s <= 40: buckets['21-40'] += 1
    elif s <= 60: buckets['41-60'] += 1
    elif s <= 80: buckets['61-80'] += 1
    else: buckets['81-100'] += 1

result = {
    'total_records': len(raw_data),
    'active_high_count': len(active_high),
    'active_high_ids': sorted([d['id'] for d in active_high]),
    'tag_averages': tag_avg,
    'score_distribution': buckets,
    'overall_average': round(sum(d['score'] for d in raw_data) / len(raw_data), 1),
}
print(json.dumps(result))""",
             "timeout_ms": 30000},
    check=_check_stdout_all(['"total_records": 50', '"active_high_count":',
                            '"score_distribution":', '"overall_average":']),
)

test(
    name="python_config_parser",
    description="Python: env config parsing with validation and structured output",
    category="Production-like Scripts",
    payload={"runtime": "python", "code": """import json, os, re

# Simulate environment variable config parsing
raw_config = {
    'APP_NAME': 'my-service',
    'PORT': '8080',
    'DB_HOST': 'localhost',
    'DB_PORT': '5432',
    'DB_NAME': 'sandbox',
    'LOG_LEVEL': 'info',
    'MAX_CONNECTIONS': '100',
    'TIMEOUT_SECONDS': '30',
    'FEATURE_FLAG_X': 'true',
    'ALLOWED_ORIGINS': 'http://localhost:3000,https://app.example.com',
    'RATE_LIMIT': '1000',
}

# Validation rules
validators = {
    'PORT': lambda v: 1024 <= int(v) <= 65535,
    'DB_PORT': lambda v: 1024 <= int(v) <= 65535,
    'MAX_CONNECTIONS': lambda v: int(v) > 0,
    'TIMEOUT_SECONDS': lambda v: int(v) > 0,
    'RATE_LIMIT': lambda v: int(v) > 0,
    'FEATURE_FLAG_X': lambda v: v in ('true', 'false'),
    'LOG_LEVEL': lambda v: v in ('debug', 'info', 'warn', 'error'),
}

config = {}
errors = []
for key, raw_val in raw_config.items():
    try:
        val = raw_val
        if key in ('PORT', 'DB_PORT', 'MAX_CONNECTIONS', 'TIMEOUT_SECONDS', 'RATE_LIMIT'):
            val = int(val)
        if key == 'FEATURE_FLAG_X':
            val = val == 'true'
        if key == 'ALLOWED_ORIGINS':
            val = [o.strip() for o in val.split(',')]
        if key in validators and not validators[key](raw_val):
            errors.append(f'{key}: validation failed')
        config[key.lower()] = val
    except (ValueError, TypeError) as e:
        errors.append(f'{key}: {e}')

result = {
    'app_name': config['app_name'],
    'port': config['port'],
    'db': {'host': config.get('db_host'), 'port': config.get('db_port'), 'name': config.get('db_name')},
    'limits': {'max_connections': config.get('max_connections'), 'timeout_s': config.get('timeout_seconds'), 'rate': config.get('rate_limit')},
    'features': {'flag_x': config.get('feature_flag_x')},
    'allowed_origins': config.get('allowed_origins', []),
    'validation_errors': errors,
    'config_keys_parsed': len(config),
}
print(json.dumps(result, indent=2))""",
             "timeout_ms": 30000},
    check=_check_stdout_all(['"app_name": "my-service"', '"port": 8080',
                            '"validation_errors": []', '"config_keys_parsed": 11']),
)

test(
    name="python_log_analyzer",
    description="Python: structured log analysis with regex and stats",
    category="Production-like Scripts",
    payload={"runtime": "python", "code": """import json, re, statistics

LOG_LINES = \"\"\"2024-01-15T10:00:01 INFO  request_id=abc123 method=GET path=/api/users status=200 duration_ms=45
2024-01-15T10:00:02 WARN  request_id=abc124 method=POST path=/api/orders status=400 duration_ms=12 body='invalid payload'
2024-01-15T10:00:03 ERROR request_id=abc125 method=GET path=/api/items status=500 duration_ms=5023 body='db timeout'
2024-01-15T10:00:04 INFO  request_id=abc126 method=GET path=/api/users status=200 duration_ms=23
2024-01-15T10:00:05 INFO  request_id=abc127 method=PUT path=/api/users/42 status=200 duration_ms=67
2024-01-15T10:00:06 ERROR request_id=abc128 method=POST path=/api/orders status=503 duration_ms=12034 body='service unavailable'
2024-01-15T10:00:07 WARN  request_id=abc129 method=GET path=/api/items status=404 duration_ms=8
2024-01-15T10:00:08 INFO  request_id=abc130 method=DELETE path=/api/sessions status=204 duration_ms=5
2024-01-15T10:00:09 INFO  request_id=abc131 method=GET path=/api/users status=200 duration_ms=31
2024-01-15T10:00:10 ERROR request_id=abc132 method=POST path=/api/auth status=500 duration_ms=8923 body='auth failure'
\"\"\"

pattern = re.compile(
    r'(\\S+) (\\S+) (\\S+) method=(\\S+) path=(\\S+) status=(\\d+) duration_ms=(\\d+)')

entries = []
for line in LOG_LINES.strip().split('\\n'):
    m = pattern.search(line)
    if m:
        entries.append({
            'timestamp': m.group(1),
            'level': m.group(2),
            'request_id': m.group(3),
            'method': m.group(4),
            'path': m.group(5),
            'status': int(m.group(6)),
            'duration_ms': int(m.group(7)),
        })

# Compute analytics
levels = {}
methods = {}
status_codes = {}
durations = []
slow_requests = []
for e in entries:
    levels[e['level']] = levels.get(e['level'], 0) + 1
    methods[e['method']] = methods.get(e['method'], 0) + 1
    status_codes[e['status']] = status_codes.get(e['status'], 0) + 1
    durations.append(e['duration_ms'])
    if e['duration_ms'] > 1000:
        slow_requests.append(e)

result = {
    'total_entries': len(entries),
    'levels': levels,
    'methods': methods,
    'status_codes': status_codes,
    'duration_stats': {
        'min': min(durations),
        'max': max(durations),
        'avg': round(sum(durations) / len(durations), 1),
        'median': statistics.median(durations),
    },
    'slow_request_count': len(slow_requests),
    'error_rate_pct': round(levels.get('ERROR', 0) / len(entries) * 100, 1),
}
print(json.dumps(result, indent=2))""",
             "timeout_ms": 30000},
    check=_check_stdout_all(['"total_entries": 10', '"levels":', '"methods":', '"error_rate_pct": 30.0']),
)

test(
    name="python_api_response",
    description="Python: generate paginated JSON API response",
    category="Production-like Scripts",
    payload={"runtime": "python", "code": """import json, math

ITEMS = [
    {'id': i, 'name': f'Product {i}', 'price': round(9.99 + (i * 1.5), 2),
     'category': 'Electronics' if i % 3 == 0 else 'Clothing' if i % 3 == 1 else 'Food',
     'in_stock': i % 5 != 0, 'rating': round(3.0 + (i % 20) * 0.2, 1)}
    for i in range(1, 48)
]

page = 2
per_page = 10
start = (page - 1) * per_page
end = start + per_page
page_items = ITEMS[start:end]

total_pages = math.ceil(len(ITEMS) / per_page)

def get_category_counts(items):
    counts = {}
    for item in items:
        c = item['category']
        counts[c] = counts.get(c, 0) + 1
    return counts

def get_price_range(items):
    prices = [i['price'] for i in items]
    return {'min': min(prices), 'max': max(prices), 'avg': round(sum(prices) / len(prices), 2)}

response = {
    'status': 'success',
    'pagination': {
        'page': page,
        'per_page': per_page,
        'total_items': len(ITEMS),
        'total_pages': total_pages,
        'has_next': end < len(ITEMS),
        'has_prev': page > 1,
    },
    'data': page_items,
    'summary': {
        'categories_on_page': get_category_counts(page_items),
        'price_range': get_price_range(page_items),
        'in_stock_count': sum(1 for i in page_items if i['in_stock']),
        'avg_rating': round(sum(i['rating'] for i in page_items) / len(page_items), 1),
    }
}
print(json.dumps(response, indent=2))""",
             "timeout_ms": 30000},
    check=_check_stdout_all(['"status": "success"', '"page": 2', '"total_items": 47',
                            '"has_next": true', '"has_prev": true']),
)

# ── Node.js: Production-like processing ───────────────────────────────────────

test(
    name="node_data_pipeline",
    description="Node.js: array data transformation pipeline (map/filter/reduce)",
    category="Production-like Scripts",
    payload={"runtime": "node", "code": """// Simulate ETL-style data transformation
const rawData = Array.from({length: 200}, (_, i) => ({
  id: i + 1,
  value: Math.floor(Math.random() * 1000),
  group: ['A', 'B', 'C', 'D'][i % 4],
  active: i % 7 !== 0,
  timestamp: Date.now() + i * 1000
}));

// Filter active records
const active = rawData.filter(d => d.active);

// Group by 'group' and compute aggregates
const grouped = active.reduce((acc, d) => {
  if (!acc[d.group]) acc[d.group] = {count: 0, total: 0, values: []};
  acc[d.group].count++;
  acc[d.group].total += d.value;
  acc[d.group].values.push(d.value);
  return acc;
}, {});

for (const [g, stats] of Object.entries(grouped)) {
  stats.avg = Math.round(stats.total / stats.count);
  stats.min = Math.min(...stats.values);
  stats.max = Math.max(...stats.values);
  delete stats.values;
}

// Overall stats
const values = active.map(d => d.value);
const sum = values.reduce((a, b) => a + b, 0);
const avg = Math.round(sum / values.length);
const sorted = [...values].sort((a, b) => a - b);
const median = sorted.length % 2 === 0
  ? (sorted[sorted.length/2 - 1] + sorted[sorted.length/2]) / 2
  : sorted[Math.floor(sorted.length/2)];

const result = {
  total_records: rawData.length,
  active_count: active.length,
  groups: Object.keys(grouped).sort(),
  group_stats: grouped,
  overall: { sum, avg, median, min: sorted[0], max: sorted[sorted.length - 1] }
};

console.log(JSON.stringify(result, null, 2));""",
             "timeout_ms": 30000},
    check=_check_stdout_all(['"total_records": 200', '"groups":', '"group_stats":', '"overall":']),
)

test(
    name="node_config_loader",
    description="Node.js: configuration loading with validation and defaults",
    category="Production-like Scripts",
    payload={"runtime": "node", "code": """// Simulate a config loader with env var parsing, defaults, and validation
const rawConfig = {
  APP_NAME: 'data-processor',
  NODE_ENV: 'production',
  PORT: '3000',
  DB_URL: 'postgres://user:pass@localhost:5432/mydb',
  REDIS_HOST: 'redis.local',
  REDIS_PORT: '6379',
  LOG_LEVEL: 'info',
  MAX_RETRIES: '3',
  BATCH_SIZE: '1000',
  ENABLE_CACHE: 'true',
  ALLOWED_IPS: '10.0.0.0/8,172.16.0.0/12',
  RATE_LIMIT_WINDOW_MS: '60000',
  RATE_LIMIT_MAX: '100',
};

const defaults = {
  port: 8080,
  logLevel: 'debug',
  maxRetries: 0,
  batchSize: 500,
  enableCache: false,
  rateLimitWindowMs: 60000,
  rateLimitMax: 50,
};

const validators = {
  port: v => v >= 1024 && v <= 65535,
  maxRetries: v => v >= 0 && v <= 10,
  batchSize: v => v > 0 && v <= 10000,
  rateLimitMax: v => v > 0,
};

function parseConfig(raw) {
  const config = { ...defaults };
  const errors = [];

  if (raw.PORT) config.port = parseInt(raw.PORT, 10);
  if (raw.LOG_LEVEL) config.logLevel = raw.LOG_LEVEL.toLowerCase();
  if (raw.MAX_RETRIES) config.maxRetries = parseInt(raw.MAX_RETRIES, 10);
  if (raw.BATCH_SIZE) config.batchSize = parseInt(raw.BATCH_SIZE, 10);
  if (raw.ENABLE_CACHE) config.enableCache = raw.ENABLE_CACHE === 'true';
  if (raw.RATE_LIMIT_WINDOW_MS) config.rateLimitWindowMs = parseInt(raw.RATE_LIMIT_WINDOW_MS, 10);
  if (raw.RATE_LIMIT_MAX) config.rateLimitMax = parseInt(raw.RATE_LIMIT_MAX, 10);
  if (raw.ALLOWED_IPS) config.allowedIps = raw.ALLOWED_IPS.split(',').map(s => s.trim());
  if (raw.DB_URL) config.db = { url: raw.DB_URL };
  if (raw.REDIS_HOST) config.redis = { host: raw.REDIS_HOST, port: parseInt(raw.REDIS_PORT || '6379', 10) };

  for (const [key, validate] of Object.entries(validators)) {
    if (config[key] !== undefined && !validate(config[key])) {
      errors.push(`Validation failed for ${key}: ${config[key]}`);
    }
  }

  config.validationErrors = errors;
  config.isValid = errors.length === 0;
  return config;
}

const config = parseConfig(rawConfig);
console.log(JSON.stringify(config, null, 2));""",
             "timeout_ms": 30000},
    check=_check_stdout_all(['"port": 3000', '"logLevel": "info"', '"isValid": true',
                            '"validationErrors": []', '"db":', '"redis":']),
)

test(
    name="node_json_api",
    description="Node.js: generate paginated JSON API with nested resources",
    category="Production-like Scripts",
    payload={"runtime": "node", "code": """// Generate a paginated JSON API response with nested resources
const products = Array.from({length: 55}, (_, i) => ({
  id: `prod_${i + 1}`,
  sku: `SKU-${String(i + 1).padStart(5, '0')}`,
  name: `Product ${i + 1}`,
  price: parseFloat((9.99 + i * 2.5).toFixed(2)),
  category: ['Electronics', 'Clothing', 'Home', 'Sports'][i % 4],
  inStock: i % 7 !== 0,
  variants: [
    { size: 'S', stock: Math.floor(Math.random() * 50) },
    { size: 'M', stock: Math.floor(Math.random() * 50) },
    { size: 'L', stock: Math.floor(Math.random() * 50) }
  ],
  reviews: Math.floor(Math.random() * 200),
  rating: parseFloat((3 + Math.random() * 2).toFixed(1))
}));

const page = 2;
const perPage = 10;
const start = (page - 1) * perPage;
const end = start + perPage;
const pageItems = products.slice(start, end);

const totalPages = Math.ceil(products.length / perPage);

const categoryCounts = pageItems.reduce((acc, p) => {
  acc[p.category] = (acc[p.category] || 0) + 1;
  return acc;
}, {});

const prices = pageItems.map(p => p.price);
const avgPrice = prices.reduce((a, b) => a + b, 0) / prices.length;

const response = {
  statusCode: 200,
  headers: { 'content-type': 'application/json' },
  pagination: {
    page,
    perPage,
    totalItems: products.length,
    totalPages,
    hasNextPage: end < products.length,
    hasPrevPage: page > 1
  },
  data: pageItems,
  summary: {
    categoriesOnPage: categoryCounts,
    avgPrice: parseFloat(avgPrice.toFixed(2)),
    minPrice: Math.min(...prices),
    maxPrice: Math.max(...prices),
    inStockCount: pageItems.filter(p => p.inStock).length,
    avgRating: parseFloat((pageItems.reduce((s, p) => s + p.rating, 0) / pageItems.length).toFixed(1))
  }
};

console.log(JSON.stringify(response, null, 2));""",
             "timeout_ms": 30000},
    check=_check_stdout_all(['"statusCode": 200', '"page": 2', '"totalItems": 55',
                            '"hasNextPage": true', '"hasPrevPage": true', '"categoriesOnPage":']),
)

test(
    name="node_file_batch",
    description="Node.js: batch file creation, transformation, and summary",
    category="Production-like Scripts",
    payload={"runtime": "node", "code": """const fs = require('fs');
const path = require('path');

// Create working directory
fs.mkdirSync('invoices', { recursive: true });

// Generate invoice files
const customers = ['Acme Corp', 'Globex Inc', 'Initech', 'Hooli', 'Stark Industries'];
const items = ['Widget', 'Gadget', 'Service', 'License', 'Consulting'];

for (let i = 1; i <= 20; i++) {
  const customer = customers[i % customers.length];
  const lineItems = [];
  const itemCount = 1 + (i % 4);
  let total = 0;
  for (let j = 0; j < itemCount; j++) {
    const qty = 1 + (i + j) % 10;
    const price = parseFloat((10 + (i * j) % 100).toFixed(2));
    total += qty * price;
    lineItems.push({ item: items[(i + j) % items.length], qty, price });
  }
  const invoice = {
    id: `INV-${String(i).padStart(4, '0')}`,
    customer,
    date: `2024-01-${String(1 + (i % 28)).padStart(2, '0')}`,
    items: lineItems,
    total: parseFloat(total.toFixed(2))
  };
  fs.writeFileSync(`invoices/invoice_${i}.json`, JSON.stringify(invoice, null, 2));
}

// Read all invoices and compute summary
const files = fs.readdirSync('invoices').filter(f => f.endsWith('.json'));
let grandTotal = 0;
let maxInvoice = null;
const customerTotals = {};

files.forEach(f => {
  const data = JSON.parse(fs.readFileSync(path.join('invoices', f), 'utf8'));
  grandTotal += data.total;
  customerTotals[data.customer] = (customerTotals[data.customer] || 0) + data.total;
  if (!maxInvoice || data.total > maxInvoice.total) maxInvoice = data;
});

const result = {
  invoiceCount: files.length,
  grandTotal: parseFloat(grandTotal.toFixed(2)),
  uniqueCustomers: Object.keys(customerTotals).length,
  topCustomer: Object.entries(customerTotals).sort((a, b) => b[1] - a[1])[0][0],
  topCustomerTotal: parseFloat(Math.max(...Object.values(customerTotals)).toFixed(2)),
  largestInvoice: maxInvoice ? maxInvoice.id : null,
  largestInvoiceTotal: maxInvoice ? maxInvoice.total : null
};

console.log(JSON.stringify(result, null, 2));

// Cleanup
fs.rmSync('invoices', { recursive: true, force: true });""",
             "timeout_ms": 30000},
    check=_check_stdout_all(['"invoiceCount": 20', '"uniqueCustomers": 5', '"grandTotal":']),
)

test(
    name="node_event_processing",
    description="Node.js: event emitter with async processing pipeline",
    category="Production-like Scripts",
    payload={"runtime": "node", "code": """const EventEmitter = require('events');

// Simulate an event-driven processing pipeline
class OrderProcessor extends EventEmitter {
  constructor() { super(); this.orders = []; this.metrics = { processed: 0, failed: 0, totalValue: 0 }; }

  async processOrder(order) {
    this.emit('orderReceived', order);
    try { await this.validate(order); this.emit('orderValidated', order); }
    catch (e) { this.emit('orderFailed', order, e.message); return; }
    try { await this.fulfill(order); this.emit('orderFulfilled', order); }
    catch (e) { this.emit('orderFailed', order, e.message); }
  }

  async validate(order) {
    if (!order.items || order.items.length === 0) throw new Error('No items');
    if (order.total <= 0) throw new Error('Invalid total');
    return true;
  }

  async fulfill(order) {
    this.orders.push(order);
    this.metrics.processed++;
    this.metrics.totalValue += order.total;
  }

  getMetrics() { return { ...this.metrics, averageOrderValue: parseFloat((this.metrics.totalValue / this.metrics.processed || 0).toFixed(2)) }; }
}

const processor = new OrderProcessor();
const events = [];

processor.on('orderReceived', o => events.push(`received:${o.id}`));
processor.on('orderValidated', o => events.push(`validated:${o.id}`));
processor.on('orderFulfilled', o => events.push(`fulfilled:${o.id}`));
processor.on('orderFailed', (o, reason) => events.push(`failed:${o.id}:${reason}`));

const orders = [
  { id: 'ORD-001', items: [{ sku: 'A1', qty: 2 }], total: 50.00 },
  { id: 'ORD-002', items: [], total: 0 },
  { id: 'ORD-003', items: [{ sku: 'B2', qty: 1 }], total: 25.00 },
  { id: 'ORD-004', items: [{ sku: 'C3', qty: 5 }], total: 150.00 },
  { id: 'ORD-005', items: [{ sku: 'D4', qty: 3 }], total: -10.00 },
];

(async () => {
  for (const order of orders) {
    await processor.processOrder(order);
  }
  const metrics = processor.getMetrics();
  console.log(JSON.stringify({ metrics, eventLog: events }, null, 2));
})();""",
             "timeout_ms": 30000},
    check=_check_stdout_all(['"processed": 3', '"failed": 2', '"totalValue": 225', '"averageOrderValue": 75']),
)

# ═══════════════════════════════════════════════════════════════════════════════
#  Benchmark definitions
# ═══════════════════════════════════════════════════════════════════════════════

BENCHMARKS = [
    {
        "name": "bash_trivial",
        "description": "Bash: trivial echo (latency floor)",
        "payload": {"runtime": "bash", "code": "echo hi", "timeout_ms": 30000},
    },
    {
        "name": "python_trivial",
        "description": "Python: trivial print (startup + latency)",
        "payload": {"runtime": "python", "code": "print('hi')", "timeout_ms": 30000},
    },
    {
        "name": "node_trivial",
        "description": "Node.js: trivial log (startup + latency)",
        "payload": {"runtime": "node", "code": "console.log('hi')", "timeout_ms": 30000},
    },
    {
        "name": "bash_heavy_output",
        "description": "Bash: 10,000 lines of output",
        "payload": {"runtime": "bash",
                    "code": "for i in $(seq 1 10000); do echo 'line '$i; done",
                    "timeout_ms": 30000},
    },
    {
        "name": "python_heavy_cpu",
        "description": "Python: 5M square sum (heavy CPU)",
        "payload": {"runtime": "python",
                    "code": "print(f'sum={sum(i*i for i in range(5_000_000))}')",
                    "timeout_ms": 60000},
    },
    {
        "name": "node_heavy_json",
        "description": "Node.js: 100K JSON serialization",
        "payload": {"runtime": "node",
                    "code": "const d=[]; for(let i=0;i<100000;i++) d.push({i, s:'x'.repeat(20)}); console.log(JSON.stringify(d).length);",
                    "timeout_ms": 60000},
    },

    # ── Production-like benchmarks ────────────────────────────────────────

    {
        "name": "bash_log_parse_pipeline",
        "description": "Bash: Apache log parsing pipeline (awk/sort/uniq)",
        "payload": {"runtime": "bash",
                    "code": "cat > access.log << 'LOGDATA'\n192.168.1.1 - - [10/Jan/2024:08:00:01 +0000] \"GET /api/users HTTP/1.1\" 200 1234\n192.168.1.2 - - [10/Jan/2024:08:00:05 +0000] \"GET /api/items HTTP/1.1\" 404 56\n192.168.1.1 - - [10/Jan/2024:08:00:10 +0000] \"POST /api/users HTTP/1.1\" 201 78\n192.168.1.3 - - [10/Jan/2024:08:00:15 +0000] \"GET /api/users HTTP/1.1\" 200 5678\n192.168.1.1 - - [10/Jan/2024:08:00:20 +0000] \"GET /api/users HTTP/1.1\" 500 12\n192.168.1.4 - - [10/Jan/2024:08:00:25 +0000] \"DELETE /api/sessions HTTP/1.1\" 204 0\n192.168.1.2 - - [10/Jan/2024:08:00:30 +0000] \"GET /api/items HTTP/1.1\" 304 0\n192.168.1.5 - - [10/Jan/2024:08:00:35 +0000] \"POST /api/auth HTTP/1.1\" 200 234\n192.168.1.1 - - [10/Jan/2024:08:00:40 +0000] \"GET /api/users HTTP/1.1\" 200 89\n192.168.1.3 - - [10/Jan/2024:08:00:45 +0000] \"PUT /api/users/42 HTTP/1.1\" 200 45\nLOGDATA\nTOTAL=$(wc -l < access.log)\nUNIQUE_IPS=$(awk '{print $1}' access.log | sort -u | wc -l)\nOK200=$(grep -c ' 200 ' access.log)\nCLIENT_ERR=$(grep -c ' 4[0-9][0-9] ' access.log)\nSERVER_ERR=$(grep -c ' 5[0-9][0-9] ' access.log)\nTOP_IP=$(awk '{print $1}' access.log | sort | uniq -c | sort -rn | head -1 | awk '{print $2}')\nTOP_IP_COUNT=$(awk '{print $1}' access.log | sort | uniq -c | sort -rn | head -1 | awk '{print $1}')\necho \"total=$TOTAL unique_ips=$UNIQUE_IPS ok=$OK200 client_errors=$CLIENT_ERR server_errors=$SERVER_ERR top_ip=$TOP_IP top_count=$TOP_IP_COUNT\"\nrm -f access.log",
                    "timeout_ms": 30000},
    },
    {
        "name": "bash_csv_pipeline_sales",
        "description": "Bash: CSV sales data pipeline (awk aggregation)",
        "payload": {"runtime": "bash",
                    "code": "cat > orders.csv << 'CSV'\norder_id,customer,amount,region,date\nORD001,Alice,150.00,NA,2024-01-15\nORD002,Bob,275.50,EU,2024-01-16\nORD003,Alice,89.99,NA,2024-02-01\nORD004,Charlie,450.00,APAC,2024-02-05\nORD005,Bob,32.50,EU,2024-02-10\nORD006,Alice,210.00,NA,2024-02-15\nORD007,Diana,675.00,NA,2024-03-01\nORD008,Charlie,120.00,APAC,2024-03-05\nORD009,Bob,500.00,EU,2024-03-10\nORD010,Diana,85.00,NA,2024-03-15\nCSV\necho \"top_cust=$(awk -F',' 'NR>1 {a[$2]+=$3} END{for(c in a) print a[c],c}' orders.csv | sort -rn | head -1 | awk '{print $2}')\"\necho \"top_spend=$(awk -F',' 'NR>1 {a[$2]+=$3} END{for(c in a) print a[c],c}' orders.csv | sort -rn | head -1 | awk '{print $1}')\"\necho \"regions=$(awk -F',' 'NR>1 {a[$4]+=$3} END{for(r in a) printf \"%s:%.0f \", r, a[r]}' orders.csv)\"\necho \"order_count=$(( $(wc -l < orders.csv) - 1 ))\"\necho \"total_rev=$(awk -F',' 'NR>1 {s+=$3} END{printf \"%.2f\", s}' orders.csv)\"\nrm -f orders.csv",
                    "timeout_ms": 30000},
    },
    {
        "name": "python_csv_analysis",
        "description": "Python: CSV employee analysis (DictReader/aggregation)",
        "payload": {"runtime": "python",
                    "code": "import csv, io, json\n\nCSV_DATA = \"\"\"name,role,salary,department\nAlice,Engineer,95000,Engineering\nBob,Designer,82000,Design\nCharlie,Engineer,105000,Engineering\nDiana,Manager,115000,Engineering\nEve,Designer,78000,Design\nFrank,Engineer,98000,Engineering\nGrace,Intern,45000,Engineering\nHenry,Manager,125000,Design\n\"\"\"\n\nreader = csv.DictReader(io.StringIO(CSV_DATA))\nrows = list(reader)\n\ndept_stats = {}\nfor r in rows:\n    d = r['department']\n    dept_stats.setdefault(d, {'count': 0, 'total': 0.0, 'roles': set()})\n    dept_stats[d]['count'] += 1\n    dept_stats[d]['total'] += float(r['salary'])\n    dept_stats[d]['roles'].add(r['role'])\n\nhigh_earners = [r for r in rows if float(r['salary']) > 90000]\nhigh_earners.sort(key=lambda x: float(x['salary']), reverse=True)\n\nresult = {\n    'employee_count': len(rows),\n    'departments': list(dept_stats.keys()),\n    'dept_avg_salary': {d: round(s['total']/s['count'], 2) for d, s in dept_stats.items()},\n    'high_earners_count': len(high_earners),\n    'top_earner': high_earners[0]['name'] if high_earners else None,\n    'unique_roles': sorted(set(r['role'] for r in rows)),\n}\nprint(json.dumps(result, indent=2))",
                    "timeout_ms": 30000},
    },
    {
        "name": "python_config_parse",
        "description": "Python: env config parsing with validation (11 vars)",
        "payload": {"runtime": "python",
                    "code": "import json\n\nraw_config = {\n    'APP_NAME': 'my-service', 'PORT': '8080', 'DB_HOST': 'localhost', 'DB_PORT': '5432',\n    'DB_NAME': 'sandbox', 'LOG_LEVEL': 'info', 'MAX_CONNECTIONS': '100',\n    'TIMEOUT_SECONDS': '30', 'FEATURE_FLAG_X': 'true',\n    'ALLOWED_ORIGINS': 'http://localhost:3000,https://app.example.com', 'RATE_LIMIT': '1000',\n}\n\nvalidators = {\n    'PORT': lambda v: 1024 <= int(v) <= 65535, 'DB_PORT': lambda v: 1024 <= int(v) <= 65535,\n    'MAX_CONNECTIONS': lambda v: int(v) > 0, 'TIMEOUT_SECONDS': lambda v: int(v) > 0,\n    'RATE_LIMIT': lambda v: int(v) > 0, 'LOG_LEVEL': lambda v: v in ('debug', 'info', 'warn', 'error'),\n}\n\nconfig = {}\nerrors = []\nfor key, raw_val in raw_config.items():\n    try:\n        val = raw_val\n        if key in ('PORT', 'DB_PORT', 'MAX_CONNECTIONS', 'TIMEOUT_SECONDS', 'RATE_LIMIT'):\n            val = int(val)\n        if key == 'FEATURE_FLAG_X': val = val == 'true'\n        if key == 'ALLOWED_ORIGINS': val = [o.strip() for o in val.split(',')]\n        if key in validators and not validators[key](raw_val):\n            errors.append(f'{key}: validation failed')\n        config[key.lower()] = val\n    except (ValueError, TypeError) as e:\n        errors.append(f'{key}: {e}')\n\nresult = {\n    'app_name': config['app_name'], 'port': config['port'],\n    'db': {'host': config.get('db_host'), 'port': config.get('db_port'), 'name': config.get('db_name')},\n    'limits': {'max_connections': config.get('max_connections'), 'timeout_s': config.get('timeout_seconds'), 'rate': config.get('rate_limit')},\n    'allowed_origins': config.get('allowed_origins', []), 'validation_errors': errors,\n    'config_keys_parsed': len(config),\n}\nprint(json.dumps(result, indent=2))",
                    "timeout_ms": 30000},
    },
    {
        "name": "node_data_etl",
        "description": "Node.js: ETL data transform (200 recs, filter/group/stats)",
        "payload": {"runtime": "node",
                    "code": "const rawData = Array.from({length: 200}, (_, i) => ({\n  id: i + 1,\n  value: Math.floor(Math.random() * 1000),\n  group: ['A', 'B', 'C', 'D'][i % 4],\n  active: i % 7 !== 0\n}));\n\nconst active = rawData.filter(d => d.active);\n\nconst grouped = active.reduce((acc, d) => {\n  if (!acc[d.group]) acc[d.group] = {count: 0, total: 0, values: []};\n  acc[d.group].count++;\n  acc[d.group].total += d.value;\n  acc[d.group].values.push(d.value);\n  return acc;\n}, {});\n\nfor (const [g, stats] of Object.entries(grouped)) {\n  stats.avg = Math.round(stats.total / stats.count);\n  stats.min = Math.min(...stats.values);\n  stats.max = Math.max(...stats.values);\n  delete stats.values;\n}\n\nconst values = active.map(d => d.value);\nconst sorted = [...values].sort((a, b) => a - b);\nconst median = sorted.length % 2 === 0\n  ? (sorted[sorted.length/2 - 1] + sorted[sorted.length/2]) / 2\n  : sorted[Math.floor(sorted.length/2)];\n\nconsole.log(JSON.stringify({\n  total_records: rawData.length, active_count: active.length,\n  groups: Object.keys(grouped).sort(), group_stats: grouped,\n  overall: { sum: values.reduce((a,b)=>a+b,0), avg: Math.round(values.reduce((a,b)=>a+b,0)/values.length), median, min: sorted[0], max: sorted[sorted.length-1] }\n}));",
                    "timeout_ms": 30000},
    },
    {
        "name": "node_file_batch",
        "description": "Node.js: batch file create/read/aggregate (20 invoices)",
        "payload": {"runtime": "node",
                    "code": "const fs = require('fs');\nconst path = require('path');\n\nfs.mkdirSync('invoices', { recursive: true });\n\nconst customers = ['Acme Corp', 'Globex Inc', 'Initech', 'Hooli', 'Stark Industries'];\n\nfor (let i = 1; i <= 20; i++) {\n  const customer = customers[i % customers.length];\n  const lineItems = [];\n  let total = 0;\n  for (let j = 0; j < 1 + (i % 4); j++) {\n    const qty = 1 + (i + j) % 10;\n    const price = parseFloat((10 + (i * j) % 100).toFixed(2));\n    total += qty * price;\n    lineItems.push({ item: ['Widget','Gadget','Service','License','Consulting'][(i + j) % 5], qty, price });\n  }\n  fs.writeFileSync(`invoices/invoice_${i}.json`, JSON.stringify({\n    id: `INV-${String(i).padStart(4,'0')}`, customer,\n    date: `2024-01-${String(1+(i%28)).padStart(2,'0')}`, items: lineItems,\n    total: parseFloat(total.toFixed(2))\n  }));\n}\n\nconst files = fs.readdirSync('invoices').filter(f => f.endsWith('.json'));\nlet grandTotal = 0;\nlet maxInvoice = null;\nconst customerTotals = {};\n\nfiles.forEach(f => {\n  const data = JSON.parse(fs.readFileSync(path.join('invoices', f), 'utf8'));\n  grandTotal += data.total;\n  customerTotals[data.customer] = (customerTotals[data.customer] || 0) + data.total;\n  if (!maxInvoice || data.total > maxInvoice.total) maxInvoice = data;\n});\n\nconsole.log(JSON.stringify({\n  invoiceCount: files.length,\n  grandTotal: parseFloat(grandTotal.toFixed(2)),\n  uniqueCustomers: Object.keys(customerTotals).length,\n  topCustomer: Object.entries(customerTotals).sort((a,b)=>b[1]-a[1])[0][0],\n  largestInvoice: maxInvoice ? maxInvoice.id : null\n}));\n\nfs.rmSync('invoices', { recursive: true, force: true });",
                    "timeout_ms": 30000},
    },
]


# ═══════════════════════════════════════════════════════════════════════════════
#  Invocation helpers
# ═══════════════════════════════════════════════════════════════════════════════

def invoke_aws_cli(function_arn: str, region: str, profile: str,
                    payload: dict) -> dict:
    """Invoke Lambda via `aws lambda invoke` CLI and return parsed response."""
    import tempfile
    with tempfile.NamedTemporaryFile(mode="w", suffix=".json", delete=False) as f:
        json.dump(payload, f)
        payload_path = f.name

    out_path = payload_path + ".out"
    cmd = [
        "aws", "--profile", profile,
        "lambda", "invoke",
        "--region", region,
        "--function-name", function_arn,
        "--cli-binary-format", "raw-in-base64-out",
        "--payload", f"fileb://{payload_path}",
        out_path,
    ]

    try:
        result = subprocess.run(cmd, capture_output=True, text=True, timeout=120)
        if result.returncode != 0:
            return {"_error": f"AWS CLI error: {result.stderr.strip()}"}

        with open(out_path) as f:
            body = json.load(f)
        body["_status_code"] = 200
        return body
    except subprocess.TimeoutExpired:
        return {"_error": "CLI invocation timed out (120s)"}
    except json.JSONDecodeError as e:
        return {"_error": f"Invalid JSON response: {e}"}
    finally:
        for p in [payload_path, out_path]:
            try:
                os.unlink(p)
            except OSError:
                pass


def invoke_url(url: str, payload: dict) -> dict:
    """Invoke Lambda via Function URL using `requests`."""
    try:
        import requests  # type: ignore # noqa: F811
    except ImportError:
        return {"_error": "requests library required for --url mode. pip install requests"}

    try:
        resp = requests.post(url, json=payload, timeout=120)
        body = resp.json()
        body["_status_code"] = resp.status_code
        return body
    except Exception as e:
        return {"_error": f"HTTP error: {e}"}


# ═══════════════════════════════════════════════════════════════════════════════
#  Report generation
# ═══════════════════════════════════════════════════════════════════════════════

PASS_EMOJI = "✅"
FAIL_EMOJI = "❌"
SKIP_EMOJI = "⏭️"


def _duration_bar(ms: float, max_ms: float = 5000) -> str:
    """Simple text bar for duration visualization."""
    if max_ms <= 0:
        max_ms = 5000
    bar_len = 20
    ratio = min(ms / max_ms, 1.0)
    filled = int(ratio * bar_len)
    bar = "█" * filled + "░" * (bar_len - filled)
    return f"{bar} {ms:>7.1f}ms"


def generate_report(results: list[dict], bench_results: list[dict],
                    function_arn: str, region: str,
                    wall_seconds: float) -> str:
    """Produce a complete markdown report."""
    lines = []
    def sep():
        return lines.append("")

    def h1(t): lines.append(f"# {t}")
    def h2(t): lines.append(f"## {t}")
    def h3(t): lines.append(f"### {t}")
    def code(t): lines.append(f"```\n{t}\n```")
    def p(t): lines.append(t)

    # ── Header ───────────────────────────────────────────────────────────
    h1("🧪 Lambda Agent Sandbox — Test & Benchmark Report")
    sep()
    p(f"**Generated:** {datetime.now(timezone.utc).strftime('%Y-%m-%d %H:%M:%S UTC')}")
    p(f"**Function:** `{function_arn}`")
    p(f"**Region:** `{region}`")
    p(f"**Total wall time:** `{wall_seconds:.1f}s`")
    sep()
    p("---")
    sep()

    # ── Overall summary ───────────────────────────────────────────────────
    total = len(results)
    passed = sum(1 for r in results if r["pass"])
    failed = total - passed
    rate = (passed / total * 100) if total else 0

    h2("📊 Overall Results")
    sep()
    p("| Metric | Value |")
    p("|--------|-------|")
    p(f"| **Total Tests** | {total} |")
    p(f"| **Passed** | {passed} |")
    p(f"| **Failed** | {failed} |")
    p(f"| **Pass Rate** | {rate:.1f}% |")
    p(f"| **Benchmarks** | {len(bench_results)} |")
    sep()

    # ── By category ──────────────────────────────────────────────────────
    categories = {}
    for r in results:
        cat = r.get("category", "Uncategorized")
        categories.setdefault(cat, {"total": 0, "passed": 0})
        categories[cat]["total"] += 1
        if r["pass"]:
            categories[cat]["passed"] += 1

    h2("📁 Results by Category")
    sep()
    p("| Category | Tests | Passed | Failed | Pass Rate |")
    p("|---|---|---|---|---|")
    for cat, stats in sorted(categories.items()):
        fail = stats["total"] - stats["passed"]
        cr = (stats["passed"] / stats["total"] * 100) if stats["total"] else 0
        p(f"| {cat} | {stats['total']} | {stats['passed']} | {fail} | {cr:.0f}% |")
    sep()

    # ── Detailed test results ─────────────────────────────────────────────
    h2("🔍 Detailed Test Results")
    sep()

    current_cat = None
    for r in results:
        if r["category"] != current_cat:
            current_cat = r["category"]
            h3(f"### {current_cat}")
            sep()

        icon = PASS_EMOJI if r["pass"] else FAIL_EMOJI
        detail = r.get("detail", "").strip()
        dur = r.get("duration_ms")
        dur_str = f" — `{dur}ms`" if dur is not None else ""

        p(f"- {icon} **{r['name']}:** {r['description']}{dur_str}")
        if detail:
            p(f"  - _{detail}_")
        sep()

    # ── Benchmark results ─────────────────────────────────────────────────
    h2("⚡ Performance Benchmarks")
    sep()

    if bench_results:
        all_durs = []
        for b in bench_results:
            if b.get("all_runs"):
                all_durs.extend(b["all_runs"])

        max_bench_dur = max(all_durs) if all_durs else 5000

        p("| Benchmark | Result | Avg | Cold | Warm Avg | Bar (avg) |")
        p("|---|---|---|---|---|---|")
        for b in bench_results:
            dur = b.get("duration_ms", 0)
            ok_str = PASS_EMOJI if b.get("ok") else FAIL_EMOJI
            bar = _duration_bar(dur, max_bench_dur)
            cold_str = f"{b['cold_duration_ms']:.0f}ms" if b.get("cold_duration_ms") is not None else "—"
            warm_str = f"{b['warm_avg_ms']:.0f}ms" if b.get("warm_avg_ms") is not None else "—"
            p(f"| {b['name']} | {ok_str} | {dur:.0f}ms | {cold_str} | {warm_str} | {bar} |")
        sep()

        # Cold vs warm analysis
        h3("❄️  Cold vs 🔥 Warm Start Analysis")
        sep()

        cold_benchmarks = [b for b in bench_results if b.get("is_cold_start")]
        if cold_benchmarks:
            p("**Identified cold starts** (first run faster >1.5x median of all runs):")
            for b in cold_benchmarks:
                p(f"- {b['name']}: cold `{b['cold_duration_ms']:.0f}ms` vs "
                  f"warm avg `{b['warm_avg_ms']:.0f}ms` "
                  f"({b['cold_duration_ms'] / b['warm_avg_ms']:.1f}x slowdown)")
            sep()

        # Aggregate warm duration statistics across all benchmarks
        all_warm = []
        for b in bench_results:
            if b.get("warm_durations_ms"):
                all_warm.extend(b["warm_durations_ms"])
        if all_warm:
            warm_avg = sum(all_warm) / len(all_warm)
            p(f"**Aggregate warm invocation stats** (across {len(all_warm)} runs):")
            p(f"- **Average:** `{warm_avg:.0f}ms`")
            p(f"- **Median:** `{median(all_warm):.0f}ms`")
            sorted_warm = sorted(all_warm)
            p95_idx = min(int(len(sorted_warm) * 0.95), len(sorted_warm) - 1)
            p(f"- **P95:** `{sorted_warm[p95_idx]:.0f}ms`")
            if len(all_warm) > 2:
                p(f"- **Std deviation:** `{stdev(all_warm):.0f}ms`")
            sep()

        # Per-runtime cold start breakdown
        h3("Per-Runtime Cold Start Impact")
        sep()
        runtime_groups = {
            "Bash": [b for b in bench_results if "Bash:" in b["name"]],
            "Python": [b for b in bench_results if "Python:" in b["name"]],
            "Node.js": [b for b in bench_results if "Node.js:" in b["name"]],
        }
        p("| Runtime | Benchmark | Cold (ms) | Warm Avg (ms) | Slowdown |")
        p("|---|---|---|---|---|")
        for rt, benches in runtime_groups.items():
            for b in benches:
                c = b.get("cold_duration_ms")
                w = b.get("warm_avg_ms")
                if c and w:
                    ratio = c / w
                    arrow = "🚀" if ratio < 1.3 else "❄️" if ratio < 2.0 else "🧊"
                    p(f"| {rt} | {b['name']} | {c:.0f} | {w:.0f} | {arrow} {ratio:.1f}x |")
                elif c:
                    p(f"| {rt} | {b['name']} | {c:.0f} | — | — |")
            if not benches:
                p(f"| {rt} | — | — | — | — |")
        sep()
    else:
        p("*No benchmark data collected.*")
        sep()

    # ── System info ───────────────────────────────────────────────────────
    h2("💻 Environment Info")
    sep()
    p("| Property | Value |")
    p("|----------|-------|")
    p(f"| Python | `{sys.version}` |")
    p(f"| Platform | `{sys.platform}` |")
    p(f"| Timestamp | `{datetime.now(timezone.utc).isoformat()}` |")
    sep()

    # ── Footer ────────────────────────────────────────────────────────────
    p("---")
    p(f"*Report generated by `scripts/benchmark_sandbox.py` — {total} tests, "
      f"{passed} passed, {failed} failed ({rate:.0f}% success)*")
    sep()

    return "\n".join(lines)


# ═══════════════════════════════════════════════════════════════════════════════
#  JSON output builder
# ═══════════════════════════════════════════════════════════════════════════════

def _build_json_output(results: list[dict], bench_results: list[dict],
                        function_arn: str, region: str,
                        wall_seconds: float, env_file: Optional[str]) -> dict:
    """Build a structured JSON object for programmatic validation."""
    total = len(results)
    passed = sum(1 for r in results if r["pass"])
    failed = total - passed
    rate = (passed / total * 100) if total else 0

    # Group by category
    categories = {}
    for r in results:
        cat = r.get("category", "Uncategorized")
        categories.setdefault(cat, {"total": 0, "passed": 0, "failed": 0})
        categories[cat]["total"] += 1
        if r["pass"]:
            categories[cat]["passed"] += 1
        else:
            categories[cat]["failed"] += 1

    # Build benchmark summary
    bench_summary = []
    for b in bench_results:
        entry = {
            "name": b["name"],
            "ok": b.get("ok"),
            "avg_duration_ms": round(b.get("duration_ms", 0), 1),
            "all_runs_ms": [round(d, 1) for d in b.get("all_runs", [])],
            "cold_duration_ms": round(b["cold_duration_ms"], 1) if b.get("cold_duration_ms") is not None else None,
            "warm_avg_ms": round(b["warm_avg_ms"], 1) if b.get("warm_avg_ms") is not None else None,
            "is_cold_start": b.get("is_cold_start", False),
        }
        bench_summary.append(entry)

    return {
        "metadata": {
            "tool": "lambda-agent-sandbox-benchmark",
            "version": "1.0.0",
            "timestamp": datetime.now(timezone.utc).isoformat(),
            "wall_time_seconds": round(wall_seconds, 1),
            "env_file": env_file,
        },
        "target": {
            "function_arn": function_arn if function_arn.startswith("arn:") else None,
            "url": function_arn if function_arn.startswith("http") else None,
            "region": region,
        },
        "summary": {
            "total_tests": total,
            "passed": passed,
            "failed": failed,
            "pass_rate_pct": round(rate, 1),
            "total_benchmarks": len(bench_results),
        },
        "categories": {
            cat: {
                "total": s["total"],
                "passed": s["passed"],
                "failed": s["failed"],
                "pass_rate_pct": round(s["passed"] / s["total"] * 100, 1) if s["total"] else 0,
            }
            for cat, s in sorted(categories.items())
        },
        "tests": [
            {
                "name": r["name"],
                "description": r["description"],
                "category": r.get("category", "Uncategorized"),
                "pass": r["pass"],
                "detail": r.get("detail", ""),
                "duration_ms": r.get("duration_ms"),
            }
            for r in results
        ],
        "benchmarks": bench_summary,
    }


# ═══════════════════════════════════════════════════════════════════════════════
#  Main
# ═══════════════════════════════════════════════════════════════════════════════

def main():
    parser = argparse.ArgumentParser(
        description="Lambda Agent Sandbox — Benchmark & Test Suite")
    group = parser.add_mutually_exclusive_group()
    group.add_argument("--function", help="Lambda function ARN")
    group.add_argument("--url", help="Lambda Function URL (alternative to --function)")
    parser.add_argument("--region", default="eu-central-1", help="AWS region")
    parser.add_argument("--profile", default="default", help="AWS CLI profile")
    parser.add_argument("--output", "-o", default="sandbox_report.md",
                        help="Output markdown report path")
    parser.add_argument("--json-output", "-j", metavar="PATH",
                        help="Write structured JSON results to PATH (for programmatic validation)")
    parser.add_argument("--env-file", "-e", metavar="PATH", default=".env",
                        help="Path to .env file with config overrides (default: ./.env)")
    parser.add_argument("--warmup", "-w", type=int, default=2,
                        help="Number of warmup invocations before benchmarking")
    parser.add_argument("--benchmark-runs", "-n", type=int, default=5,
                        help="Benchmark repetitions per test")
    args = parser.parse_args()

    # ── Load .env file ───────────────────────────────────────────────────
    env_path = args.env_file
    env = _load_env_file(env_path)
    _apply_env_to_args(args, env)

    # Must have either --function, --url, or one from .env
    if not args.function and not args.url:
        parser.error(
            "No target specified. Provide --function, --url, "
            "or set FUNCTION_ARN/LAMBDA_FUNCTION_ARN or "
            "URL/LAMBDA_URL in the .env file.")

    # Set up invoker
    if args.function:
        def invoker(payload):
            return invoke_aws_cli(
                    args.function, args.region, args.profile, payload)
        source_desc = args.function
    else:
        def invoker(payload):
            return invoke_url(args.url, payload)
        source_desc = args.url

    print("🚀 Lambda Agent Sandbox — Test Suite")
    print(f"   Target: {source_desc}")
    print(f"   Tests:  {len(TESTS)}")
    print(f"   Benchmarks: {len(BENCHMARKS)}")
    print(f"   Warmup: {args.warmup}x  |  Benchmark runs: {args.benchmark_runs}x")
    print()

    start_wall = time.time()

    # ── Warmup ────────────────────────────────────────────────────────────
    if args.warmup > 0:
        print(f"🔥 Warming up ({args.warmup}x invocations)...")
        for i in range(args.warmup):
            resp = invoker({"runtime": "bash", "code": "echo warmup",
                           "timeout_ms": 5000})
            err = resp.get("_error")
            if err:
                print(f"   ⚠️  Warmup {i+1} failed: {err}")
            else:
                print(f"   ✅ Warmup {i+1}: {resp.get('duration_ms', '?')}ms")
        print()

    # ── Run tests ─────────────────────────────────────────────────────────
    print(f"🧪 Running {len(TESTS)} tests...")
    results = []
    for idx, t in enumerate(TESTS, 1):
        name = t["name"]
        skip = t.get("skip", False)

        if skip:
            print(f"   {idx:>3}. {SKIP_EMOJI} {name} — skipped")
            results.append({
                "name": name,
                "description": t["description"],
                "category": t.get("category", "Uncategorized"),
                "pass": True,
                "detail": "Skipped",
                "duration_ms": None,
            })
            continue

        resp = invoker(t["payload"])
        error = resp.get("_error")
        duration = resp.get("duration_ms")

        if error:
            print(f"   {idx:>3}. {FAIL_EMOJI} {name} — {error}")
            results.append({
                "name": name,
                "description": t["description"],
                "category": t.get("category", "Uncategorized"),
                "pass": False,
                "detail": error,
                "duration_ms": duration,
            })
            continue

        check_fn = t["check"]
        try:
            passed, detail = check_fn(resp)
        except Exception as e:
            passed, detail = False, f"Check exception: {e}"

        icon = PASS_EMOJI if passed else FAIL_EMOJI
        print(f"   {idx:>3}. {icon} {name} — {'PASS' if passed else 'FAIL'} "
              f"({duration}ms)"
              + (f"  [{detail}]" if not passed else ""))
        results.append({
            "name": name,
            "description": t["description"],
            "category": t.get("category", "Uncategorized"),
            "pass": passed,
            "detail": detail if not passed else "",
            "duration_ms": duration,
        })

    print()

    # ── Run benchmarks ────────────────────────────────────────────────────
    print(f"⚡ Running {len(BENCHMARKS)} benchmarks ({args.benchmark_runs}x each)...")
    bench_results = []
    for bench in BENCHMARKS:
        name = bench["name"]
        all_runs: list[float] = []
        for run_idx in range(args.benchmark_runs):
            resp = invoker(bench["payload"])
            dur = resp.get("duration_ms")
            if dur is not None:
                all_runs.append(dur)
            time.sleep(0.2)  # small gap between runs

        if all_runs:
            avg_dur = sum(all_runs) / len(all_runs)
            min_dur = min(all_runs)
            max_dur = max(all_runs)
            # First run is the cold-start candidate; subsequent runs are "warm"
            cold_dur = all_runs[0]
            warm_durs = all_runs[1:]
            warm_avg = sum(warm_durs) / len(warm_durs) if warm_durs else None

            # Detect true cold start: first run > 1.5x the median of warm runs
            if len(warm_durs) >= 3:
                warm_med = sorted(warm_durs)[len(warm_durs) // 2]
                is_genuinely_cold = (cold_dur > warm_med * 1.5)
            elif len(warm_durs) >= 1:
                # With few warm runs, use a higher threshold (2x)
                warm_avg = sum(warm_durs) / len(warm_durs)
                is_genuinely_cold = (cold_dur > warm_avg * 2.0)
            else:
                is_genuinely_cold = False

            print(f"   📊 {name}: avg={avg_dur:.0f}ms  cold={cold_dur:.0f}ms"
                  + (f"  warm_avg={warm_avg:.0f}ms" if warm_avg else "")
                  + f"  (n={len(all_runs)})")
            bench_results.append({
                "name": bench["description"],
                "ok": True,
                "duration_ms": avg_dur,
                "min_ms": min_dur,
                "max_ms": max_dur,
                "all_runs": all_runs,
                "cold_duration_ms": cold_dur,
                "warm_durations_ms": warm_durs,
                "warm_avg_ms": warm_avg,
                "is_cold_start": is_genuinely_cold,
            })
        else:
            print(f"   ❌ {name}: no data")
            bench_results.append({
                "name": bench["description"],
                "ok": False,
                "duration_ms": 0,
                "cold_duration_ms": None,
                "warm_durations_ms": [],
                "warm_avg_ms": None,
                "is_cold_start": False,
            })

    print()

    # ── Generate report ───────────────────────────────────────────────────
    wall_elapsed = time.time() - start_wall
    report = generate_report(results, bench_results, args.function or args.url,
                             args.region, wall_elapsed)

    with open(args.output, "w") as f:
        f.write(report)
    print(f"📄 Markdown report saved to: {args.output}")

    # ── Generate JSON output ──────────────────────────────────────────────
    if args.json_output:
        json_data = _build_json_output(
            results, bench_results, args.function or args.url,
            args.region, wall_elapsed, args.env_file)
        with open(args.json_output, "w") as f:
            json.dump(json_data, f, indent=2)
        print(f"📊 JSON validation output saved to: {args.json_output}")

    print(f"⏱️  Total time: {wall_elapsed:.1f}s")

    # ── Exit code ─────────────────────────────────────────────────────────
    failed_count = sum(1 for r in results if not r["pass"])
    sys.exit(1 if failed_count > 0 else 0)


if __name__ == "__main__":
    main()
