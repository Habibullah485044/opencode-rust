# ============================================================
# COMPLETE v2: Ollama (2xGPU, 27B IQ4_XS) + warm-up + tunnel watchdog
#  - skips any stage whose output already exists (tag / gguf)
#  - download: clean .part resume, size verify, curl rc in the error
# ============================================================
import os, re, json, glob, time, shutil, subprocess, threading, urllib.request

# ---------- CONFIG ----------
PORT             = 11434
OLLAMA_BIN       = "/usr/local/bin/ollama"
HF_REPO          = "HauhauCS/Qwen3.8-27B-Uncensored-HauhauCS-Aggressive-MTP-GGUF"
QUANT_TOKENS     = ["IQ4_XS", "IQ4-XS", "iq4_xs"]
LOCAL_MODEL_NAME = "qwen3.8-uncensored:27b-iq4_xs"   # exact tag — use this everywhere
WORKDIR          = "/kaggle/working/ocrs"
# ----------------------------

os.makedirs(WORKDIR, exist_ok=True)
sh0 = lambda c: subprocess.run(c, shell=True, executable="/bin/bash",
                               stdin=subprocess.DEVNULL).returncode

if not os.path.exists(OLLAMA_BIN):
    sh0("apt-get update -qq && apt-get install -y -qq zstd pciutils curl wget")
    sh0("curl -fsSL https://ollama.com/install.sh | sh")

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

def sh(cmd, timeout=7200):
    try:
        p = subprocess.run(cmd, shell=True, executable="/bin/bash",
                           capture_output=True, text=True, timeout=timeout,
                           env=env, stdin=subprocess.DEVNULL)
        return p.returncode, p.stdout + p.stderr
    except subprocess.TimeoutExpired:
        return 124, "TIMEOUT after %ss" % timeout

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
sh("pkill -9 -f 'ollama serve'", timeout=10)
time.sleep(2)
subprocess.Popen(f"{OLLAMA_BIN} serve > {WORKDIR}/ollama.log 2>&1", shell=True, env=env)
for _ in range(30):
    if sh0(f"curl -sf http://127.0.0.1:{PORT}/api/version") == 0:
        break
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
    with opener.open(f"https://huggingface.co/api/models/{HF_REPO}/tree/main", timeout=60) as r:
        tree = json.loads(r.read().decode())
    entry = next((f for f in tree
                  if f["path"].lower().endswith(".gguf")
                  and any(t.lower() in os.path.basename(f["path"]).lower() for t in QUANT_TOKENS)),
                 None)
    if not entry:
        raise RuntimeError(f"no IQ4_XS gguf in repo; paths={[f['path'] for f in tree]}")
    gguf_path, gguf_size = entry["path"], entry.get("size", 0)
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
        url = f"https://huggingface.co/{HF_REPO}/resolve/main/{gguf_path}"
        part = local_gguf + ".part"
        # attempt 1: resume if a partial exists, attempt 2: from scratch
        for attempt, resume in enumerate([os.path.exists(part), False], 1):
            flag = "-C -" if resume else ""
            rc, log = sh(f'curl -fL {flag} --retry 5 --retry-all-errors '
                         f'--speed-limit 50000 --speed-time 60 '
                         f'-o "{part}" "{url}"', timeout=7200)
            got = os.path.getsize(part) if os.path.exists(part) else 0
            print(f"attempt {attempt}: rc={rc} got={got/1e9:.2f} GB", flush=True)
            if rc == 0 and got == gguf_size:
                os.replace(part, local_gguf)
                break
            if rc == 0:  # complete-size mismatch -> restart clean
                os.remove(part)
            else:
                print("curl said: " + log.strip().splitlines()[-1][:300], flush=True)
        else:
            raise RuntimeError(
                f"download failed after retries — curl rc={rc}; "
                f"last output: {log.strip().splitlines()[-1][:300] if log.strip() else 'empty'}")
        if not os.path.exists(local_gguf):
            raise RuntimeError("download produced no file")
    print(f"gguf: {os.path.getsize(local_gguf)/1e9:.2f} GB ✔", flush=True)

    # 3d) create with the EXACT tag
    print("=== creating model ===", flush=True)
    mf = f"{WORKDIR}/Modelfile"
    open(mf, "w").write(f'FROM "{local_gguf}"\nPARAMETER temperature 0.7\n')
    rc, log = sh(f'ollama create {LOCAL_MODEL_NAME} -f {mf}', timeout=3600)
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

# ---------- 5) Tunnel + watchdog ----------
print("=== Tunnel ===", flush=True)
if not os.path.exists(f"{WORKDIR}/cloudflared"):
    sh('curl -fL --retry 3 -o %s/cloudflared '
       'https://github.com/cloudflare/cloudflared/releases/latest/download/cloudflared-linux-amd64' % WORKDIR)
    sh0(f"chmod +x {WORKDIR}/cloudflared")
sh("pkill -f cloudflared", timeout=10)
time.sleep(1)
LOG = f"{WORKDIR}/tunnel.log"
open(LOG, "w").close()

def tunnel_proc():
    return subprocess.Popen(
        f"{WORKDIR}/cloudflared tunnel --url http://127.0.0.1:{PORT} "
        f"--no-autoupdate >> {LOG} 2>&1", shell=True, env=env)

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
            print('  $env:OPENCODE_RUST_API_KEY="none"', flush=True)
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
