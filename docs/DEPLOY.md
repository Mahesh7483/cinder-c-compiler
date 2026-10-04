# Deploying the playground on Render

The whole playground (compiler, API, web UI) is **one Docker image** built from the `Dockerfile` in the
repository root, so it is a single Render *Web Service* with the **Docker** runtime. The repository also contains
a Blueprint, [`render.yaml`](../render.yaml), that describes that service, so you can deploy with a couple of
clicks or create the service by hand — both are below.

> **Status of this guide.** The image, the health check and the sandbox were tested locally with Docker's default
> security profile (the same one GitHub Actions runners use). It has not been run on Render itself from this
> repository: the "Verify the deployment" section below tells you how to confirm in thirty seconds that Render's
> runtime supports the full sandbox, and what to do if it does not. Dashboard labels
> are quoted from Render's documentation and may be reworded over time.

## 0. What you need

* A GitHub (or GitLab/Bitbucket) repository containing this project, pushed to a branch you want to deploy
  (`main`).
* A Render account (<https://render.com>). The **Free** instance type is enough.

```bash
git remote add origin git@github.com:<you>/<repo>.git
git push -u origin main
```

## Option A — Blueprint (recommended)

1. In the Render Dashboard click **New +** → **Blueprint**.
2. Connect your Git provider if asked and pick the repository. Render finds `render.yaml` in the root.
3. Give the Blueprint a name (e.g. `cinder`), check that it lists one web service, `cinder-playground`,
   with the `free` plan, and click **Apply** (or *Deploy Blueprint*).
4. Render builds the image. The first build compiles the Rust workspace in release mode and takes several
   minutes; later builds reuse cached layers when only `web/` or docs change.
5. When the service shows **Live**, open its URL (`https://cinder-playground-<suffix>.onrender.com`).

Every push to the deployed branch redeploys automatically (`autoDeployTrigger: commit`). Changing
`render.yaml` and pushing updates the service's settings through the Blueprint.

## Option B — create the service by hand

1. **New +** → **Web Service** → *Build and deploy from a Git repository* → pick the repository.
2. Fill in:

   | field | value |
   |-------|-------|
   | Name | `cinder-playground` |
   | Language / Runtime | **Docker** |
   | Branch | `main` |
   | Region | the one closest to your users |
   | Dockerfile Path | `./Dockerfile` (default) |
   | Docker Build Context Directory | `.` (default) |
   | Instance Type | **Free** |

3. Open **Advanced** and set **Health Check Path** to `/api/health`.
4. Under **Environment Variables** add the variables from the table below. Only `SANDBOX` is essential; the
   image already carries sensible defaults for the rest. Do **not** set `PORT`: Render provides it (default
   `10000`) and the server reads it.
5. Click **Create Web Service**.

## Environment variables

Set by `render.yaml` (and baked into the image as defaults). The authoritative list with defaults is the table
in `crates/cinder-server/src/config.rs`.

| variable | value on Render | meaning |
|----------|-----------------|---------|
| `SANDBOX` | `require` | run programs only if the full sandbox (chroot + uid + seccomp + rlimits) passes the start-up self-test; otherwise running is disabled and `/api/health` says why. Never `off` on a public server |
| `RUN_CPU_SECS` | `2` | CPU-time limit of a program (`SIGXCPU` after that) |
| `RUN_WALL_SECS` | `10` | wall-clock limit (generous on the free plan, which gives a fraction of a core) |
| `RUN_MEMORY_MB` | `128` | address space of each process and resident memory of all its processes together |
| `COMPILE_MEMORY_MB` | `160` | resident memory of one compile |
| `RUN_PROCESSES` | `16` | process/thread limit of a program |
| `OUTPUT_LIMIT_BYTES` | `65536` | stdout and stderr cap (each) |
| `MAX_CONCURRENT` | `2` | simultaneous compiles + runs (the free plan has 512 MB of RAM) |
| `RATE_RUN_PER_MIN` / `RATE_COMPILE_PER_MIN` | `20` / `60` | per-client rate limits |
| `CLIENT_IP_HEADER` | `CF-Connecting-IP` | header the edge proxy overwrites with the real client address (used for rate limiting) |
| `TRUST_PROXY_HOPS` | `1` | fallback: take the client from `X-Forwarded-For` this many entries from the right |
| `QUEUE_WAIT_SECS`, `COMPILE_TIMEOUT_SECS`, `MAX_CODE_BYTES`, `MAX_STDIN_BYTES`, `RUN_UID_BASE` | defaults | see `config.rs` |

## Verify the deployment

1. Open `https://<your-service>.onrender.com/api/health`. You should see

   ```json
   {"runEnabled":true,"sandbox":"chroot+uid+seccomp+rlimits","status":"ok","version":"0.1.0"}
   ```

   The service's **Logs** tab shows the same decision at start-up:
   `sandbox: chroot+uid+seccomp+rlimits (self-test passed)`.
2. Open the root URL, load an example, press **Run** (Ctrl/Cmd+Enter): the program's output appears in the console.
3. Paste `int main(void) { for (;;); }` and Run: after ~2 s the console reports it was terminated by `SIGXCPU`.
4. Click **Share** and open the copied link in a private window: the same program and settings load.

### If `/api/health` says something else

| you see | meaning | what to do |
|---------|---------|------------|
| `"sandbox":"disabled"`, `"runEnabled":false` | the self-test failed under `SANDBOX=require`; the log line names the failing step (usually a denied `chroot`, `setuid` or seccomp call) | the compiler views still work. Check **Logs** for the reason. If Render's runtime does not allow `chroot`/`setuid`, set `SANDBOX=auto` to accept the next-best mode (`uid+seccomp+rlimits`, still never root) — read [SANDBOX.md](SANDBOX.md) first and decide whether that isolation is enough for your audience |
| `"sandbox":"seccomp+rlimits"` | the server is not running as root | the image runs as root on purpose; check no `USER` instruction was added |
| the deploy fails at *Build* | usually a transient network error fetching crates | **Manual Deploy → Clear build cache & deploy** |
| the first request after a while takes ~1 min | free instances spin down after 15 minutes without traffic | expected on the free plan; a paid instance type stays up |

## Things to know about the free plan

* Free web services spin down after 15 minutes without inbound traffic and take about a minute to start again; they
  get 750 instance-hours per month and have an ephemeral filesystem (fine: the playground stores nothing; share links
  live in the URL).
* The rate limiter and the slot pool are in memory, so run **one** instance (free services cannot scale anyway).
* A free instance has a fraction of a CPU core, so heavy programs run slower than on a laptop; that is why
  `RUN_WALL_SECS` is higher than the 2 s CPU limit. On a paid instance type you can raise `MAX_CONCURRENT` and
  `RUN_CPU_SECS` together with the memory limits.
* Render's current instance specs and limits: <https://render.com/docs/free> and the pricing page.

## Updating and rolling back

Push to the branch. In the dashboard, **Events** shows each deploy and **Rollback** returns to a previous one.
To redeploy without a code change use **Manual Deploy**.

## Running the same image elsewhere

```bash
docker build -t cinder-playground .
docker run --rm -p 8080:8080 cinder-playground           # needs Docker's default capabilities (CHROOT, SETUID, SETGID)
```

Any host that runs the container with the default Docker capability set gets the same full sandbox; if you drop
`CAP_SYS_CHROOT` or `CAP_SETUID` the server reports the weaker mode (or disables running) instead of pretending.
