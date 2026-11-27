#!/usr/bin/env python3
"""Bundled llama.cpp GGUF deployment worker (executes ONLY on the Colab VM).

Reads `PLUGIN_INPUT_*` and optional `PLUGIN_HF_TOKEN`, downloads prebuilt
llama.cpp CUDA binaries + the best-fitting GGUF quantisation, preserves
`/usr/lib64-nvidia` on `LD_LIBRARY_PATH` to prevent silent CPU fallback,
warms up CUDA kernels and writes atomic status. Downloads and inference
update the VM watchdog heartbeat when they make progress.
"""

import hashlib
import json
import os
import pathlib
import re
import shutil
import socket
import signal
import subprocess
import sys
import tarfile
import tempfile
import time
import unicodedata
import urllib.error
import urllib.parse
import urllib.request

E = os.environ.get
RUN_DIR = E("PLUGIN_RUN_DIR", "/root/.colab-mcp/runs/manual")
STATUS_FILE = E("PLUGIN_STATUS_FILE", os.path.join(RUN_DIR, "status.json"))
HEARTBEAT = E("PLUGIN_HEARTBEAT", "/root/.colab-mcp/heartbeat")
REPO = E("PLUGIN_INPUT_REPO", "").strip()
QUANT = (E("PLUGIN_INPUT_QUANT", "auto") or "auto").strip()
CTX = int(float(E("PLUGIN_INPUT_CTX", "0") or 0))
PORT = int(float(E("PLUGIN_INPUT_PORT", "8000") or 8000))
ALIAS = E("PLUGIN_INPUT_ALIAS", "").strip()
KV = (E("PLUGIN_INPUT_KV", "auto") or "auto").strip()
API_KEY = E("PLUGIN_INPUT_API_KEY", "")
HF_TOKEN = E("PLUGIN_HF_TOKEN", "")
NVIDIA_LIB = "/usr/lib64-nvidia"
BIN_DIR = "/content/llama-bin"
MODEL_DIR = "/content/models"

_server_proc = None


def touch_heartbeat():
    if not HEARTBEAT:
        return
    try:
        os.makedirs(os.path.dirname(HEARTBEAT), exist_ok=True)
        with open(HEARTBEAT, "a"):
            os.utime(HEARTBEAT, None)
    except OSError:
        pass


def public_text(message):
    for secret in (API_KEY, HF_TOKEN):
        if secret:
            message = message.replace(secret, "***")
    return re.sub(r"(?i)bearer [^\s]+|hf_[A-Za-z0-9]{12,}", "***", message)


def write_status(state, stage, progress=None, message="", outputs=None):
    if state == "running":
        touch_heartbeat()
    payload = {
        "state": state,
        "stage": stage,
        "progress": progress,
        "message": public_text(message),
        "outputs": outputs or {},
        "updated_at": time.time(),
    }
    tmp = STATUS_FILE + ".tmp"
    os.makedirs(os.path.dirname(STATUS_FILE), exist_ok=True)
    with open(tmp, "w", encoding="utf-8") as fh:
        json.dump(payload, fh)
    os.replace(tmp, STATUS_FILE)


def handle_term(*_):
    raise SystemExit(0)


def detect_gpu():
    out = subprocess.check_output(
        ["nvidia-smi", "--query-gpu=name,memory.total", "--format=csv,noheader,nounits"],
        text=True,
        timeout=15,
    ).strip()
    parts = [p.strip() for p in out.splitlines()[0].split(",")]
    return parts[0], int(float(parts[1]))


def pick_gguf_files(repo, vram_mib, requested_quant):
    tree = hf_get_json(f"https://huggingface.co/api/models/{repo}/tree/main?recursive=true")
    by_q = {}
    for item in tree:
        if item.get("type") != "file":
            continue
        path = item.get("path", "")
        if not path.lower().endswith(".gguf") or "mmproj" in path.lower():
            continue
        size = (item.get("lfs") or {}).get("size") or item.get("size") or 0
        stem = re.sub(r"-\d{5}-of-\d{5}$", "", os.path.basename(path)[:-5])
        toks = re.split(r"[-.]", stem)
        q = next((t.upper() for t in reversed(toks) if re.match(r"^(IQ|TQ|Q)\d|^(F16|BF16|F32)$", t, re.I)), None)
        if not q:
            continue
        entry = by_q.setdefault(q, {"quant": q, "files": [], "size": 0})
        entry["files"].append((path, size))
        entry["size"] += size
    variants = sorted(by_q.values(), key=lambda v: (-v["size"], v["quant"]))
    for v in variants:
        v["files"].sort()
    if requested_quant.lower() != "auto":
        for v in variants:
            if v["quant"].lower() == requested_quant.lower():
                return v
    budget_bytes = int(vram_mib * 1024 * 1024 * 0.78)
    quantised = [v for v in variants if v["quant"] not in ("F16", "BF16", "F32")] or variants
    for v in quantised:
        if v["size"] <= budget_bytes:
            return v
    return quantised[-1] if quantised else None


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, *args, **kwargs):
        return None


HTTP = urllib.request.build_opener(urllib.request.ProxyHandler({}), NoRedirect())
LLAMA_TAG = "b12224"
# Published SHA-256 digests from ggml-org/llama.cpp build attestation 65373769.
RELEASES = {
    f"llama-{LLAMA_TAG}-bin-ubuntu-cuda-12.8-x64.tar.gz":
        "45bd81d4a46eb25c1d4ea9bbf755c0ff609632e3b9606814770fc2460d9738da",
    f"cudart-llama-{LLAMA_TAG}-bin-ubuntu-cuda-12.8-x64.tar.gz":
        "c1dfe6426e0548c9d15a8e99917a8ff9654d6729ece6006503cce75aa747565f",
}
MAX_ARCHIVE = 2 * 1024**3
MAX_EXPANDED = 4 * 1024**3
MAX_MEMBERS = 1024
STATUS_LIMIT = 8 * 1024**2
CHUNK = 1024**2
DOWNLOAD_LIMIT = 45 * 60


def release_host(host):
    return host in ("github.com", "release-assets.githubusercontent.com", "objects.githubusercontent.com")


def hf_host(host):
    return host == "huggingface.co" or host.endswith(".hf.co")


def open_https(url, allowed_host, *, accept=None, hf_token=None):
    for _ in range(6):
        target = urllib.parse.urlsplit(url)
        if (target.scheme != "https" or not target.hostname or not allowed_host(target.hostname)
                or target.username is not None or target.password is not None
                or target.port not in (None, 443)
                or any(c.isspace() or ord(c) < 32 or c == "\\" for c in url)):
            raise RuntimeError("Rejected download endpoint")
        headers = {"User-Agent": "colab-mcp"}
        if accept:
            headers["Accept"] = accept
        if hf_token and target.hostname == "huggingface.co":
            headers["Authorization"] = f"Bearer {hf_token}"
        try:
            return HTTP.open(urllib.request.Request(url, headers=headers), timeout=30)
        except urllib.error.HTTPError as exc:
            with exc:
                location = exc.headers.get("Location")
                if exc.code not in (301, 302, 303, 307, 308) or not location:
                    raise RuntimeError(f"Download failed with HTTP {exc.code}") from None
            url = urllib.parse.urljoin(url, location)
    raise RuntimeError("Too many download redirects")


def download(url, target, allowed_host, max_bytes, *, token=None):
    deadline = time.monotonic() + DOWNLOAD_LIMIT
    total = 0
    with open_https(url, allowed_host, hf_token=token) as response, open(target, "xb") as output:
        next_update = time.monotonic()
        while chunk := response.read(CHUNK):
            total += len(chunk)
            now = time.monotonic()
            if total > max_bytes or now >= deadline:
                raise RuntimeError("Download exceeded its size or time limit")
            output.write(chunk)
            if now >= next_update:
                touch_heartbeat()
                next_update = now + 25
    return total


def hf_get_json(url):
    with open_https(url, hf_host, accept="application/json", hf_token=HF_TOKEN) as resp:
        data = resp.read(STATUS_LIMIT + 1)
    if len(data) > STATUS_LIMIT:
        raise RuntimeError("Hugging Face metadata exceeded its size limit")
    return json.loads(data)


def sha256(path):
    digest = hashlib.sha256()
    with open(path, "rb") as source:
        for data in iter(lambda: source.read(CHUNK), b""):
            digest.update(data)
    return digest.hexdigest()


def archive_files(archive, target):
    """Extract verified executable and libraries without creating archive links."""
    links = {}
    seen = set()
    total = 0
    with tarfile.open(archive, "r|gz") as tf:
        for count, member in enumerate(tf, 1):
            total += member.size
            if count > MAX_MEMBERS or total > MAX_EXPANDED or member.size < 0:
                raise RuntimeError("llama.cpp archive exceeded extraction limits")
            path = pathlib.PurePosixPath(member.name)
            if path.is_absolute() or ".." in path.parts or "\\" in member.name:
                raise RuntimeError("Unsafe llama.cpp archive path")
            if member.isdir():
                continue
            if not (member.isfile() or member.issym() or member.islnk()):
                raise RuntimeError("Unsupported llama.cpp archive member")
            name = path.name
            if name != "llama-server" and not re.fullmatch(r"lib[A-Za-z0-9_.+-]+[.]so(?:[.][0-9]+)*", name):
                continue
            if name in seen:
                raise RuntimeError("Duplicate binary or library in llama.cpp archive")
            seen.add(name)
            if member.issym() or member.islnk():
                link = pathlib.PurePosixPath(member.linkname)
                if link.is_absolute() or ".." in link.parts or not link.name or name == "llama-server":
                    raise RuntimeError("Unsafe llama.cpp archive link")
                links[name] = link.name
                continue
            with tf.extractfile(member) as source, (target / name).open("xb") as output:
                shutil.copyfileobj(source, output, CHUNK)
            (target / name).chmod(0o700 if name == "llama-server" else 0o600)
    # Copy only validated library aliases as hard links to files inside this private directory.
    while links:
        resolved = {name: source for name, source in links.items()
                    if source in seen and source not in links and (target / source).is_file()}
        if not resolved:
            raise RuntimeError("Unresolved llama.cpp library alias")
        for name, source in resolved.items():
            os.link(target / source, target / name)
            del links[name]


def ensure_llama_server(target):
    cache = pathlib.Path(BIN_DIR)
    cache.mkdir(mode=0o700, parents=True, exist_ok=True)
    for asset, expected in RELEASES.items():
        cached = cache / asset
        # Validate the exact private copy that we extract. A cached executable is never reused.
        with tempfile.TemporaryDirectory(dir=RUN_DIR, prefix="archive-") as staging:
            archive = pathlib.Path(staging) / asset
            cached_ok = (cached.is_file() and not cached.is_symlink()
                         and cached.stat().st_size <= MAX_ARCHIVE)
            if cached_ok:
                shutil.copyfile(cached, archive)
                cached_ok = sha256(archive) == expected
            if not cached_ok:
                archive.unlink(missing_ok=True)
                url = f"https://github.com/ggml-org/llama.cpp/releases/download/{LLAMA_TAG}/{asset}"
                download(url, archive, release_host, MAX_ARCHIVE)
                if sha256(archive) != expected:
                    raise RuntimeError("llama.cpp archive SHA-256 mismatch")
                # Cache is optional and never consulted without digest verification.
                with tempfile.NamedTemporaryFile(dir=cache, delete=False) as entry:
                    staged = pathlib.Path(entry.name)
                    try:
                        with archive.open("rb") as source:
                            shutil.copyfileobj(source, entry, CHUNK)
                        entry.flush()
                        os.replace(staged, cached)
                    finally:
                        staged.unlink(missing_ok=True)
            archive_files(archive, target)
    binary = target / "llama-server"
    if not binary.is_file():
        raise RuntimeError("Verified release did not contain llama-server")
    return str(binary)


def download_model(repo, files):
    target = pathlib.Path(MODEL_DIR) / repo.replace("/", "__")
    target.mkdir(mode=0o700, parents=True, exist_ok=True)
    local_paths = []
    for idx, (name, size) in enumerate(files, 1):
        rel = pathlib.PurePosixPath(name)
        if (rel.is_absolute() or ".." in rel.parts or "\\" in name
                or not rel.name or not isinstance(size, int) or not 0 < size <= 128 * 1024**3):
            raise RuntimeError("Invalid model file path or size")
        path = target.joinpath(*rel.parts)
        if not path.resolve().is_relative_to(target.resolve()):
            raise RuntimeError("Model path escapes the model cache")
        path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
        write_status("running", "download", idx / (len(files) + 1), f"Downloading {rel.name}")
        if path.is_symlink() or not path.is_file() or path.stat().st_size != size:
            quoted = urllib.parse.quote(name, safe="/")
            with tempfile.TemporaryDirectory(dir=path.parent, prefix="download-") as work:
                staged = pathlib.Path(work) / "gguf"
                got = download(f"https://huggingface.co/{repo}/resolve/main/{quoted}",
                               staged, hf_host, size, token=HF_TOKEN)
                if got != size:
                    raise RuntimeError("Model download size differs from repository metadata")
                os.replace(staged, path)
        local_paths.append(str(path))
    return local_paths[0]


def stop_server():
    if _server_proc is None or _server_proc.poll() is not None:
        return
    _server_proc.terminate()
    try:
        _server_proc.wait(timeout=5)
    except subprocess.TimeoutExpired:
        _server_proc.kill()
        _server_proc.wait(timeout=5)


def serve_until_exit():
    # Successful inference changes the token counters. Health checks and idling do not.
    previous = None
    while _server_proc.poll() is None:
        try:
            headers = {"Authorization": f"Bearer {API_KEY}"} if API_KEY else {}
            req = urllib.request.Request(f"http://127.0.0.1:{PORT}/metrics", headers=headers)
            with HTTP.open(req, timeout=3) as response:
                metric = response.read(65536).decode()
            counters = tuple(line for line in metric.splitlines()
                             if line.startswith(("llamacpp:tokens_predicted_total ", "llamacpp:prompt_tokens_total ")))
            if previous is not None and counters and counters != previous:
                touch_heartbeat()
            previous = counters or previous
        except (OSError, ValueError):
            pass
        time.sleep(10)
    raise RuntimeError("llama-server exited; inspect server.log")


def main():
    global _server_proc
    signal.signal(signal.SIGTERM, handle_term)
    signal.signal(signal.SIGINT, handle_term)
    if not REPO:
        write_status("failed", "detect", 0.0, "Missing PLUGIN_INPUT_REPO")
        return 1
    if PORT == 8080 or not 1 <= PORT <= 65535:
        write_status("failed", "detect", 0.0, "Expected port 1–65535, excluding Colab port 8080")
        return 1
    if API_KEY.startswith("#") or any(c.isspace() or unicodedata.category(c) == "Cc" for c in API_KEY):
        write_status("failed", "detect", 0.0, "Server API key cannot start with # or contain whitespace or control characters")
        return 1

    os.umask(0o077)
    binary_dir = tempfile.TemporaryDirectory(dir=RUN_DIR, prefix="llama-bin-")
    try:
        write_status("running", "detect", 0.1, "Checking GPU")
        gpu_name, vram_mib = detect_gpu()

        write_status("running", "plan", 0.2, f"Picking a quantization ({gpu_name}, {vram_mib} MiB)")
        chosen = pick_gguf_files(REPO, vram_mib, QUANT)
        if not chosen:
            raise RuntimeError(f"No GGUF files found in {REPO}")

        write_status("running", "download", 0.35, f"Downloading model ({chosen['quant']})")
        server_bin = ensure_llama_server(pathlib.Path(binary_dir.name))
        model_path = download_model(REPO, chosen["files"])

        write_status("running", "server", 0.75, f"Starting server on port {PORT}")
        with socket.socket() as listener:
            listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
            listener.bind(("127.0.0.1", PORT))
        env = {
            "PATH": os.defpath + ":/usr/local/bin",
            "HOME": RUN_DIR,
            "LANG": "C.UTF-8",
            "PLUGIN_RUN_ID": E("PLUGIN_RUN_ID", "manual"),
            "LD_LIBRARY_PATH": f"{os.path.dirname(server_bin)}:{NVIDIA_LIB}",
        }
        model_id = ALIAS or REPO.split("/")[-1]
        cmd = [
            server_bin,
            "-m", model_path,
            "--host", "127.0.0.1",
            "--port", str(PORT),
            "--alias", model_id,
            "-ngl", "999",
            "-fa", "on",
            "--jinja",
            "--metrics",
            "-np", "1",
        ]
        if CTX > 0:
            cmd.extend(["-c", str(CTX)])
        if KV in ("f16", "q8_0", "q4_0"):
            cmd.extend(["-ctk", KV, "-ctv", KV])
        if API_KEY:
            key_file = pathlib.Path(binary_dir.name) / "api-key"
            with key_file.open("x") as key:
                key.write(API_KEY + "\n")
            cmd.extend(["--api-key-file", str(key_file)])

        with open(os.path.join(RUN_DIR, "server.log"), "ab") as log:
            _server_proc = subprocess.Popen(cmd, env=env, stdout=log, stderr=subprocess.STDOUT)

        for _ in range(90):
            if _server_proc.poll() is not None:
                raise RuntimeError("llama-server exited prematurely; inspect server.log")
            try:
                with HTTP.open(f"http://127.0.0.1:{PORT}/health", timeout=2) as r:
                    if r.status == 200:
                        break
            except Exception:
                time.sleep(2)
        else:
            raise RuntimeError("llama-server health endpoint timed out")

        if API_KEY:
            key_file.unlink()
        write_status("running", "warmup", 0.9, "Warming up")
        body = json.dumps({"model": model_id, "messages": [{"role": "user", "content": "hi"}], "max_tokens": 4}).encode()
        req_headers = {"Content-Type": "application/json"}
        if API_KEY:
            req_headers["Authorization"] = f"Bearer {API_KEY}"
        try:
            req = urllib.request.Request(f"http://127.0.0.1:{PORT}/v1/chat/completions", data=body, headers=req_headers)
            with HTTP.open(req, timeout=60) as response:
                result = json.loads(response.read(65536))
                if response.status != 200 or not isinstance(result, dict) or not result.get("choices"):
                    raise RuntimeError("llama-server did not return a completion")
        except urllib.error.HTTPError as exc:
            write_status("failed", "warmup", 0.9, f"CUDA warmup returned HTTP {exc.code}")
            return 1
        except Exception as exc:
            write_status("failed", "warmup", 0.9, f"CUDA warmup failed: {type(exc).__name__}: {exc}")
            return 1

        write_status(
            "ready",
            "warmup",
            1.0,
            f"Serving {model_id} ({chosen['quant']})",
            outputs={"model_id": model_id, "quant": chosen["quant"], "gpu": gpu_name, "port": PORT},
        )
        serve_until_exit()
        return 0
    except Exception as exc:
        write_status("failed", "server", 0.0, str(exc))
        return 1
    finally:
        stop_server()
        binary_dir.cleanup()


if __name__ == "__main__":
    sys.exit(main())
