# ============================================================
# COMPLETE v2: Ollama (2xGPU, 27B IQ4_XS) + warm-up + tunnel watchdog
#  - skips any stage whose output already exists (tag / gguf)
#  - download: clean .part resume, size verify, curl rc in the error
# ============================================================
import os, re, json, glob, time, shutil, subprocess, threading, urllib.request
import hashlib, hmac, secrets, urllib.parse, http.client, http.server

# ---------- CONFIG ----------
PORT             = 11434
OLLAMA_BIN       = "/usr/local/bin/ollama"
HF_REPO          = "HauhauCS/Qwen3.8-27B-Uncensored-HauhauCS-Aggressive-MTP-GGUF"
QUANT_TOKENS     = ["IQ4_XS", "IQ4-XS", "iq4_xs"]
LOCAL_MODEL_NAME = "qwen3.8-uncensored:27b-iq4_xs"   # exact tag — use this everywhere
WORKDIR          = "/kaggle/working/ocrs"
PROXY_PORT       = 11435   # the public tunnel points HERE (token check), never at Ollama
CLOUDFLARED_VER  = "2025.8.1"
CLOUDFLARED_SHA256 = "a66353004197ee4c1fcb68549203824882bba62378ad4d00d234bdb8251f1114"
VERIFY_GGUF_SHA256 = True  # compare the download with the SHA-256 Hugging Face publishes
# ----------------------------

os.makedirs(WORKDIR, exist_ok=True)

def run0(args):
    """Run a program with a LIST of arguments (no shell); output goes to the notebook."""
    return subprocess.run(args, stdin=subprocess.DEVNULL).returncode

if not os.path.exists(OLLAMA_BIN):
    run0(["apt-get", "update", "-qq"])
    run0(["apt-get", "install", "-y", "-qq", "zstd", "pciutils", "curl", "wget"])
    installer = f"{WORKDIR}/ollama-install.sh"
    run0(["curl", "-fsSL", "-o", installer, "https://ollama.com/install.sh"])
    run0(["sh", installer])

env = os.environ.copy()
env["PATH"] = f"/usr/local/bin:/usr/bin:/bin:{env.get('PATH', '')}"
env["NO_PROXY"] = env["no_proxy"] = "127.0.0.1,localhost"
env.update({
    "CUDA_VISIBLE_DEVICES": "0,1",
    "OLLAMA_SCHED_SPREAD": "1",
    "OLLAMA_NUM_PARALLEL": "2",
    "OLLAMA_KEEP_ALIVE": "24h",
    "OLLAMA_NUM_GPU": "999",
    "OLLAMA_CONTEXT_LENGTH": "8192",
    "OLLAMA_HOST": f"127.0.0.1:{PORT}",
})

def run(args, timeout=7200):
    """Run a program with a LIST of arguments (no shell); return (rc, output)."""
    try:
        p = subprocess.run(args, capture_output=True, text=True, timeout=timeout,
                           env=env, stdin=subprocess.DEVNULL)
        return p.returncode, p.stdout + p.stderr
    except subprocess.TimeoutExpired:
        return 124, "TIMEOUT after %ss" % timeout
    except FileNotFoundError as e:
        return 127, str(e)

opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))

def api_get(path, timeout=5):
    req = urllib.request.Request(f"http://127.0.0.1:{PORT}{path}")
    with opener.open(req, timeout=timeout) as r:
        return json.loads(r.read().decode())

def api_post(path, payload, timeout=1800):
    req = urllib.request.Request(
        f"http://127.0.0.1:{PORT}{path}", data=json.dumps(payload).encode(),
        headers={"Content-Type": "application/json"}, method="POST")
    with opener.open(req, timeout=timeout) as r:
        return json.loads(r.read().decode())

# ---------- 1) Start Ollama ----------
print("=== 1) Starting Ollama ===", flush=True)
run(["pkill", "-9", "-f", "ollama serve"], timeout=10)
time.sleep(2)
ollama_log = open(f"{WORKDIR}/ollama.log", "w")
subprocess.Popen([OLLAMA_BIN, "serve"], stdout=ollama_log, stderr=subprocess.STDOUT,
                 stdin=subprocess.DEVNULL, env=env)
for _ in range(30):
    try:
        api_get("/api/version", timeout=2)
        break
    except Exception:
        time.sleep(1)
else:
    print(open(f"{WORKDIR}/ollama.log", errors="ignore").read()[-2000:])
    raise RuntimeError("ollama serve did not come up — see log above")
print("ollama: UP ✔", flush=True)

# ---------- 2) Does the model tag already exist? ----------
tags = {t["name"] for t in api_get("/api/tags").get("models", [])}
have_model = LOCAL_MODEL_NAME in tags or f"{LOCAL_MODEL_NAME}:latest" in tags
print(f"model tag present: {have_model}", flush=True)

# ---------- 3) Get the GGUF (only if we still need to create) ----------
if have_model:
    print("skip download — tag already exists in ollama", flush=True)
else:
    # 3a) resolve file on HF
    print("=== resolving GGUF on HF ===", flush=True)
    tree, page = [], f"https://huggingface.co/api/models/{HF_REPO}/tree/main?recursive=true"
    while page:
        with opener.open(page, timeout=60) as r:
            tree += json.loads(r.read().decode())
            nxt = re.search(r'<([^>]+)>;\s*rel="next"', r.headers.get("Link", ""))
        page = nxt.group(1) if nxt else None
    entry = next((f for f in tree
                  if f.get("type") == "file" and f["path"].lower().endswith(".gguf")
                  and any(t.lower() in os.path.basename(f["path"]).lower() for t in QUANT_TOKENS)),
                 None)
    if not entry:
        raise RuntimeError(f"no IQ4_XS gguf in repo; paths={[f['path'] for f in tree]}")
    gguf_path, gguf_size = entry["path"], entry.get("size", 0)
    gguf_sha256 = (entry.get("lfs") or {}).get("oid", "")
    # The name comes from the internet and becomes a local file name and a
    # Modelfile line: accept plain names only.
    if not re.fullmatch(r"[A-Za-z0-9._-]+\.gguf", os.path.basename(gguf_path)):
        raise RuntimeError(f"refusing unexpected file name from Hugging Face: {gguf_path!r}")
    local_gguf = os.path.join(WORKDIR, os.path.basename(gguf_path))
    print(f"file: {gguf_path} ({gguf_size/1e9:.2f} GB)", flush=True)

    # 3b) reuse any existing copy under /kaggle/working
    if not (os.path.exists(local_gguf) and os.path.getsize(local_gguf) == gguf_size):
        found = next((p for p in glob.glob("/kaggle/working/**/*.gguf", recursive=True)
                      if os.path.basename(p) == os.path.basename(gguf_path)
                      and os.path.getsize(p) == gguf_size), None)
        if found and found != local_gguf:
            print(f"reusing existing copy: {found}", flush=True)
            shutil.move(found, local_gguf)

    # 3c) hardened download
    if not (os.path.exists(local_gguf) and os.path.getsize(local_gguf) == gguf_size):
        print("=== downloading GGUF ===", flush=True)
        free_gb = shutil.disk_usage(WORKDIR).free / 1e9
        print(f"free disk: {free_gb:.1f} GB (need {gguf_size/1e9 + 2:.0f})", flush=True)
        if free_gb < gguf_size / 1e9 + 2:
            raise RuntimeError(f"not enough disk: {free_gb:.1f} GB free")
        url = (f"https://huggingface.co/{HF_REPO}/resolve/main/"
               f"{urllib.parse.quote(gguf_path, safe='/')}")
        part = local_gguf + ".part"
        # attempt 1: resume if a partial exists, attempt 2: from scratch
        for attempt, resume in enumerate([os.path.exists(part), False], 1):
            args = ["curl", "-fL", "--retry", "5", "--retry-all-errors",
                    "--speed-limit", "50000", "--speed-time", "60", "-o", part, url]
            if resume:
                args[1:1] = ["-C", "-"]
            rc, log = run(args, timeout=7200)
            got = os.path.getsize(part) if os.path.exists(part) else 0
            print(f"attempt {attempt}: rc={rc} got={got/1e9:.2f} GB", flush=True)
            if rc == 0 and got == gguf_size:
                os.replace(part, local_gguf)
                break
            if rc == 0:  # complete-size mismatch -> restart clean
                os.remove(part)
            else:
                print("curl said: " + (log.strip().splitlines() or ["(no output)"])[-1][:300],
                      flush=True)
        else:
            raise RuntimeError(
                f"download failed after retries — curl rc={rc}; "
                f"last output: {log.strip().splitlines()[-1][:300] if log.strip() else 'empty'}")
        if not os.path.exists(local_gguf):
            raise RuntimeError("download produced no file")
    if VERIFY_GGUF_SHA256 and gguf_sha256:
        print("=== checking SHA-256 (takes a few minutes for big files) ===", flush=True)
        h = hashlib.sha256()
        with open(local_gguf, "rb") as f:
            for block in iter(lambda: f.read(16 << 20), b""):
                h.update(block)
        if h.hexdigest() != gguf_sha256:
            os.remove(local_gguf)
            raise RuntimeError("GGUF does not match the SHA-256 from Hugging Face — deleted, run again")
        print("sha256 ✔", flush=True)
    print(f"gguf: {os.path.getsize(local_gguf)/1e9:.2f} GB ✔", flush=True)

    # 3d) create with the EXACT tag
    print("=== creating model ===", flush=True)
    mf = f"{WORKDIR}/Modelfile"
    with open(mf, "w") as f:
        f.write(f'FROM "{local_gguf}"\nPARAMETER temperature 0.7\n')
    rc, log = run([OLLAMA_BIN, "create", LOCAL_MODEL_NAME, "-f", mf], timeout=3600)
    if rc != 0:
        raise RuntimeError(f"ollama create failed rc={rc}: {log[-800:]}")
    print(f"model: {LOCAL_MODEL_NAME} ✔", flush=True)

# ---------- 4) Warm-up ----------
print("=== Warm-up ===", flush=True)
t0 = time.time()
out = api_post("/api/chat", {
    "model": LOCAL_MODEL_NAME,
    "messages": [{"role": "user", "content": "Reply with exactly: OK"}],
    "stream": False, "keep_alive": "24h",
}, timeout=900)
print(f"warm-up done in {time.time()-t0:.0f}s -> "
      f"{out.get('message', {}).get('content', '')[:60]!r}", flush=True)

# ---------- 5) Token proxy + tunnel + watchdog ----------
# The tunnel is public. Ollama has no login, so the tunnel points at this
# small proxy, which forwards only requests that carry the token.
API_TOKEN = secrets.token_urlsafe(32)
SKIP_HEADERS = {"connection", "keep-alive", "proxy-authenticate", "proxy-authorization",
                "te", "trailers", "transfer-encoding", "upgrade", "host",
                "authorization", "content-length"}

class TokenProxy(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.0"   # the body ends when the connection closes (streams fine)

    def _forward(self):
        sent = self.headers.get("Authorization", "")
        if not hmac.compare_digest(sent.encode(), f"Bearer {API_TOKEN}".encode()):
            self.send_error(401, "missing or wrong token")
            return
        length = int(self.headers.get("Content-Length") or 0)
        body = self.rfile.read(length) if length else None
        headers = {k: v for k, v in self.headers.items() if k.lower() not in SKIP_HEADERS}
        upstream = http.client.HTTPConnection("127.0.0.1", PORT, timeout=1800)
        try:
            upstream.request(self.command, self.path, body=body, headers=headers)
            resp = upstream.getresponse()
            self.send_response(resp.status, resp.reason)
            for k, v in resp.getheaders():
                if k.lower() not in SKIP_HEADERS:
                    self.send_header(k, v)
            self.end_headers()
            while True:
                chunk = resp.read1(65536)   # returns as soon as data arrives (streaming)
                if not chunk:
                    break
                self.wfile.write(chunk)
                self.wfile.flush()
        except Exception as e:
            try:
                self.send_error(502, f"upstream error: {e}")
            except Exception:
                pass
        finally:
            upstream.close()

    do_GET = do_POST = do_PUT = do_DELETE = do_HEAD = _forward

    def log_message(self, fmt, *args):   # keep the notebook output readable
        pass

proxy = http.server.ThreadingHTTPServer(("127.0.0.1", PROXY_PORT), TokenProxy)
threading.Thread(target=proxy.serve_forever, daemon=True).start()
print(f"token proxy: 127.0.0.1:{PROXY_PORT} -> 127.0.0.1:{PORT} ✔", flush=True)

print("=== Tunnel ===", flush=True)
CF = f"{WORKDIR}/cloudflared"

def sha256_of(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for block in iter(lambda: f.read(1 << 20), b""):
            h.update(block)
    return h.hexdigest()

if not (os.path.exists(CF) and sha256_of(CF) == CLOUDFLARED_SHA256):
    rc, log = run(["curl", "-fL", "--retry", "3", "-o", CF,
                   "https://github.com/cloudflare/cloudflared/releases/download/"
                   f"{CLOUDFLARED_VER}/cloudflared-linux-amd64"])
    if rc != 0 or not os.path.exists(CF) or sha256_of(CF) != CLOUDFLARED_SHA256:
        raise RuntimeError(f"cloudflared {CLOUDFLARED_VER}: download failed or checksum mismatch (rc={rc})")
    os.chmod(CF, 0o755)
run(["pkill", "-f", "cloudflared tunnel"], timeout=10)
time.sleep(1)
LOG = f"{WORKDIR}/tunnel.log"
open(LOG, "w").close()
tunnel_log = open(LOG, "a")

def tunnel_proc():
    return subprocess.Popen(
        [CF, "tunnel", "--url", f"http://127.0.0.1:{PROXY_PORT}", "--no-autoupdate"],
        stdout=tunnel_log, stderr=subprocess.STDOUT, stdin=subprocess.DEVNULL, env=env)

def watchdog():
    while True:
        p = tunnel_proc(); p.wait()
        print("[watchdog] cloudflared died — restarting in 3s", flush=True)
        time.sleep(3)

def url_monitor():
    last = None
    while True:
        try:
            txt = open(LOG, errors="ignore").read()
        except FileNotFoundError:
            txt = ""
        urls = re.findall(r"https://[a-z0-9-]+\.trycloudflare\.com", txt)
        if urls and urls[-1] != last:
            last = urls[-1]
            print("\n===== COPY THIS TO WINDOWS =====", flush=True)
            print(f'  $env:OPENCODE_RUST_BASE_URL="{last}/v1"', flush=True)
            print(f'  $env:OPENCODE_RUST_MODEL="{LOCAL_MODEL_NAME}"', flush=True)
            print(f'  $env:OPENCODE_RUST_API_KEY="{API_TOKEN}"', flush=True)
            print(r'  .\target\release\opencode-rust.exe', flush=True)
            print("================================", flush=True)
        time.sleep(1)

threading.Thread(target=watchdog, daemon=True).start()
threading.Thread(target=url_monitor, daemon=True).start()

for _ in range(90):
    if re.search(r"https://[a-z0-9-]+\.trycloudflare\.com", open(LOG, errors="ignore").read()):
        break
    time.sleep(1)
else:
    print(open(LOG, errors="ignore").read()[-2000:])
    raise RuntimeError("tunnel failed — see log above")

print("\nALL DONE — warm model + tunnel watchdog running in background.", flush=True)
