# Cloud router — Python

Send Anthropic API traffic through Rayline's **hosted cloud router** using the
official [`anthropic`](https://pypi.org/project/anthropic/) Python SDK. The SDK
points at `https://api.rayline.ai` and authenticates with a Rayline router key.
No local process is required.

## Setup

```bash
pip install -r requirements.txt
```

## Configure

1. Generate a router key at <https://platform.rayline.ai/keys>. It looks like
   `rlk-...`.

2. Provide it either way — the example reads `RAYLINE_API_KEY` if set, otherwise
   the `API_KEY` constant in [`main.py`](main.py):

   ```bash
   export RAYLINE_API_KEY=rlk-...
   ```

   > Router keys are **bearer** tokens, so the example passes them via
   > `auth_token=`, not `api_key=`.

## Run

```bash
python main.py
```

For routing through a local model instead, see [`../../local/python`](../../local/python).
