# Cloud router — TypeScript

Send Anthropic API traffic through Rayline's **hosted cloud router** using the
official [`@anthropic-ai/sdk`](https://www.npmjs.com/package/@anthropic-ai/sdk)
package. The SDK points at `https://api.rayline.ai` and authenticates with a
Rayline router key. No local process is required.

## Setup

```bash
npm install
```

Requires Node 18+.

## Configure

1. Generate a router key at <https://platform.rayline.ai/keys>. It looks like
   `rlk-...`.

2. Provide it either way — the example reads `RAYLINE_API_KEY` if set, otherwise
   the `API_KEY` constant in [`main.ts`](main.ts):

   ```bash
   export RAYLINE_API_KEY=rlk-...
   ```

   > Router keys are **bearer** tokens, so the example passes them via
   > `authToken`, not `apiKey`.

## Run

```bash
npx tsx main.ts
```

For routing through a local model instead, see [`../../local/typescript`](../../local/typescript).
