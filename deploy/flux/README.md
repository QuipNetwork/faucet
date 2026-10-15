# `faucet.aglais.quip.network` on Flux (formerly `faucet.testnet`)

Single raw-Docker component running the CI-built faucet image, one instance, Enterprise
(encrypted) spec, with TLS terminated by Flux's FDM at a custom domain. Replaces the Akash
deployment in `../akash/` (kept for history), which cost ~$6.98/mo against ~$1.75/mo here.

## Shape, and why

- **Raw Docker, not Orbit.** Orbit builds from source in the container on every deploy and
  relocation. This binary compiles the substrate runtime — CI needs ~700s on a dedicated
  runner with a warm cargo cache — so Orbit would mean sizing (and paying for) the build
  rather than the service, plus a long rebuild in front of every restart. The image CI
  already publishes is the better artifact.
- **`instances: 1` is a correctness constraint, not a cost choice.** Every instance derives
  the *same* base wallet from the same funder mnemonic, so two instances would drive two
  nonce lanes against one account — exactly the stranded future-nonce failure QUI-723 and
  QUI-831 fixed. The abuse gate is per-process in-memory for the same reason. Flux v8 allows
  1 instance; single-instance MTTR is 65–125 min with no failover, which is the trade we're
  making knowingly.
- **Enterprise is required.** `QUIP_FAUCET_FAUCET_KEY` is the faucet authority's mnemonic
  (F on runtime 119; op-1, the chain sudo key, before it); a non-Enterprise v8 spec is
  publicly readable. Enterprise encrypts the spec
  and restricts placement to ArcaneOS nodes. Its flat +$4.00 dominates the bill, so RAM
  headroom is nearly free — hence 2048 MB for a service that ran in 512 MB on Akash.
- **No caddy, no certifier, no Tarsnap.** FDM issues and auto-renews Let's Encrypt for the
  custom domain (`portal.testnet.quip.network` has run this way since 2026-06-24, cert
  auto-renewing on a 90-day cycle). That deletes the whole cert-persistence apparatus the
  Akash deployment needed, including the manual renewal the Akash provider's SNAT forced on
  us in August 2026.
- **What FDM costs us:** it terminates TLS and re-originates cleartext, and the container
  stays directly reachable in plaintext on a high node port. Acceptable here — requests carry
  no credentials, and the gate keys on the destination account and its on-chain balance, not
  on client IP, so connecting direct buys an attacker nothing the public endpoint doesn't
  already give them.

> **Pre-119 history from here to "DNS cutover".** These sections record the August 2026
> migration, when the funder was op-1, the chain sudo key. For runtime 119 the deployable
> spec is `faucet-aglais-app-spec.json` and the procedure is "Runtime 119" below. Never
> paste op-1's mnemonic into a 119 faucet.

## Files

- `faucet-app-spec.json` — the v8 spec template. `<REPLACE-amd64-digest>` and
  `<operator-1-mnemonic>` are filled by hand into a `*.filled.json` copy, which is gitignored
  (this repo is public).

## Register

1. **Pin the image.** Take the amd64 tag *and* its digest — Flux rejects a bare
   `repo@sha256:`, it wants `repo:TAG@sha256:DIGEST`, and pinning the arch image avoids
   manifest-list resolution:

   ```bash
   glab api "projects/quip.network%2Ffaucet/registry/repositories/11457609/tags/qui1028-rc1-amd64" \
     | python3 -c 'import json,sys; print(json.load(sys.stdin)["digest"])'
   ```

2. **Fill the spec** into `faucet.filled.json`: the digest, and the mnemonic from Proton Pass
   (`OPERATOR_1_MNEMONIC`, the same value the Akash SDL used — a `0x`+64-hex seed also works).
   Nothing else needs editing. Verify no `<` remains before submitting.

3. **Register at cloud.runonflux.com** → Register New App. The Components tab accepts the
   JSON paste; Enterprise is a toggle in the General tab at submit time (never hand-write the
   `enterprise` blob). Take a **1-month first term** — Flux subscriptions are prepaid and
   non-refundable. Confirm the quote before paying:

   Confirm the quote first. The endpoint is POST-only and needs `expire` and `contacts`
   present. **Send the template, never `faucet.filled.json`** — price does not depend on env
   values and the filled copy carries the faucet authority's mnemonic.

   ```bash
   python3 -c "
   import json; s=json.load(open('faucet-app-spec.json'))
   s['enterprise']=''; s['expire']=88000
   s['contacts']=['F_S_CONTACTS=https://storage.runonflux.io/v1/contacts/94469486508667']
   print(json.dumps(s))" > /tmp/q.json
   curl -s -X POST -H 'Content-Type: text/plain' --data-binary @/tmp/q.json \
     https://api.runonflux.io/apps/calculatefiatandfluxprice
   ```

   Measured 2026-08-13, with a control arm so we know the quote discriminates:

   | spec | quoted |
   |---|---|
   | `cpu 1 / ram 2048 / hdd 1` (this app, non-enterprise) | **$0.99/mo** — the price floor |
   | `cpu 0.5 / ram 512 / hdd 1` | $0.99/mo — same floor, so downsizing saves nothing |
   | `cpu 2 / ram 8192 / hdd 20` (control) | $2.25/mo |

   The calculator can't price Enterprise without a blob, so add it by formula: enterprise is
   +4.00 on the total *before* the `/3` divisor, giving `(1.50 + 1.024 + 0.02 + 4.00) / 3 =
   2.19`, `×0.8` (instances < 4, cpu < 3, ram < 6000, hdd < 150) ≈ **$1.75/mo** against
   **$6.98/mo** on Akash. Because that flat +4.00 dominates and the non-enterprise price is
   already at the floor, the 2048 MB is free headroom — don't shave it.

4. **Verify before touching DNS.** Akash keeps serving the real hostname until step 5, so a
   broken app here costs nothing:

   ```bash
   curl -s https://quipfaucet.app.runonflux.io/health          # {"status":"ok"}
   # direct, to confirm the container itself is healthy — read the REAL published port off
   # the node, never guess it
   curl -s https://api.runonflux.io/apps/location/quipfaucet
   curl -s http://<node-ip>:<PublicPort>/health
   ```

   In the app's Logs tab expect
   `funder: 5GZMoWFMoNGLZKT1tduLMQQQC7dBQo4MHkYqriCdDATXqaYi`,
   `funder confirmed as chain sudo key`, a base wallet at `5FER…Cjs`, and `pool ready: 8`.
   Neither of the two boot-time side effects should fire: the base wallet held ~17,798 tQUIP
   on 2026-08-13 against a 10,000 tQUIP top-up threshold, so no sudo mint; and pool accounts
   0–7 are already funded, so the startup scan adopts them rather than re-funding.

   ### ⚠ Keep this overlap short — both deployments share one base wallet

   The base wallet is a fixed hard derivation off the funder SURI
   (`<funder>//faucet//base`, `src/signer.rs`), and the funder *must* be the chain sudo key or
   the startup guard aborts. So there is no way to give the Flux instance its own hot wallet:
   while Akash and Flux both run, two processes hold two in-memory nonce lanes over the *same*
   account. `submit_lane` treats the resulting collision as a stale nonce, resyncs from chain
   and retries, so it self-heals per request — but during the overlap the live Akash faucet's
   drips can cost an extra retry, and a failed verification drip here is ambiguous.

   So verify **lightly** before the cutover: `/health`, the startup log lines, and **one** EVM
   drip plus its `eth_getBalance` (that is the whole point of the release). Save the 12
   sequential + 8 concurrent soak for after the cutover, when only one faucet is serving.
   Minutes of overlap, not hours.

## DNS cutover

CNAME-first — FDM can only get an ACME cert for the custom domain once the name points at it.

1. At DNSimple **delete** the `faucet.testnet` A record (`184.105.162.182`) and create a
   **CNAME** to `quipfaucet.app.runonflux.io` (A and CNAME can't coexist at one name).
   `flux-dns-controller` does not own this record, so nothing reverts it.
2. Confirm it resolves through to `fdm-lb-*.runonflux.io`.
3. App Settings → General → **Custom Domain** → `faucet.testnet.quip.network` → Apply. This
   triggers a Soft Redeploy.
4. **Expect a ~10 minute TLS gap.** FDM serves its own `CN=runonflux.io` placeholder first
   (~7 min), then the Let's Encrypt cert lands (~8.5 min end to end when we did this for
   assets-api). Until then clients get an SNI mismatch. Early proof FDM registered the name
   before the cert arrives: plain HTTP on :80 302-redirects to the custom host.

Consumers that will log failures across that window, all of which retry:
`quip-protocol/substrate/miner_bootstrap.py`, `quip_cli.py`, `flux-cpu-nodes`
(`QUIP_FAUCET_URL`), `xquad/xqsa/tests/test_quip_live.py`, plus Better Stack and
`status.quip.network`'s checker.

## Verify

```bash
# EVM (H160) funding — the QUI-1028 feature
evm="0x$(openssl rand -hex 20)"
curl -sS -m20 -XPOST https://faucet.aglais.quip.network/request \
  -H 'content-type: application/json' -d "{\"dest\":\"$evm\",\"amount\":11000000000000}"
# 200 in ~5s; dest_account == <evm without 0x> + "ee" x 12

curl -s -XPOST https://evm-rpc.aglais.quip.network:20049 -H 'content-type: application/json' \
  -d "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"eth_getBalance\",\"params\":[\"$evm\",\"latest\"]}"
# 0x98a44c398c858000 == 10999000000000000000 wei == 10.999, NOT 11.0 — see below

# Native regression
d="0x$(openssl rand -hex 32)"
curl -sS -m20 -XPOST https://faucet.aglais.quip.network/request \
  -H 'content-type: application/json' -d "{\"dest\":\"$d\",\"amount\":11000000000000}"
# re-request the same dest past the 5s lenient window -> 403 destination already funded
# (that 403 is the proof it landed; use >10 AGLS so the max-funded gate trips)
```

Drip above 10 AGLS or the balance gate won't deny the recheck and the "did it land?" signal
disappears — the same trap that makes `quip-testnet-health`'s default 0.01 drip
false-report. On runtime 119 an 11 AGLS drip needs `--max-amount-plancks=11000000000000`
(see Runtime 119).

### `eth_getBalance` reads 0.001 tQUIP LOW, and that is correct

An 11 tQUIP drip shows as `10.999` over the EVM RPC. Don't chase it. `Pallet::evm_balance`
→ `balance_of` → `account_balance` reports the **spendable** balance, withholding the
existential deposit (`EXISTENTIAL_DEPOSIT = MILLI_UNIT = 1e9` plancks) that keeps the account
alive; the ×1e6 `NativeToEthRatio` then scales the difference into wei. Measured 2026-08-13
on the mapped account of `0xcba85cf7…08dbf`:

```
System.Account free      = 11000000000000 plancks   (exactly the drip, providers: 1, nonce 0)
(free - 1e9) x 1e6       = 10999000000000000000 wei
eth_getBalance returned  = 10999000000000000000 wei  ✓
```

So confirm the drip against `System.Account` for `dest_account` if you want the exact figure,
and treat a nonzero `eth_getBalance` as the proof that Ethereum tooling can see the funds.

## Runtime 119 (the spec-119 relaunch)

The 119 faucet mints with `FaucetOps.mint`, signed by the FaucetOps authority the Foundation
appoints with a 2-of-3 `setAuthority` motion. Sudo no longer mints. At genesis the authority
is op-1, which is also sudo, so the faucet must run on its own key F:

0. Before the switch, stop the legacy faucet and take op-1's key off the host (launch plan
   T0 step 5). On the new chain it would keep signing `Sudo::sudo(FaucetOps.mint)`, which
   119 rejects, while its `/health` still says ok.
1. Get F's account from the binary itself. Put F's mnemonic in `QUIP_FAUCET_FAUCET_KEY` (not
   on the command line), start the binary with an unreachable `--node-url`, and read the
   line `funder: <ss58> (key from QUIP_FAUCET_FAUCET_KEY)`. If it says `key from the built-in
   dev default //Alice`, the variable did not reach the process: that account must never go
   into the motion. H4 hybrid accounts differ from what other tools derive from the same
   mnemonic, so don't cross-check with them.
2. Fund F, pass `setAuthority(F)`, and wait until that block is **finalized**. The startup
   checks read the finalized head; any earlier and the faucet crash-loops on
   `configured funder is not FaucetOps.Authority`.
3. Update `quipfaucet` with the new repotag and F's mnemonic in **one** Soft Reinstall. The
   dashboard's update form arrives prefilled with the live key (op-1): replace it.

Expect, in order: `telemetry enabled …`, `funder: <F> (key from QUIP_FAUCET_FAUCET_KEY)`,
`base wallet: …`, then on first boot `base wallet low (0 < …); authority-minting …` and
`faucet minted` (an `outcome=minted` record with the extrinsic hash), `pool ready: 8
accounts` and `faucet listening …`. The `funder confirmed as chain sudo key` line no longer
exists. Every mint record has `route=base-topup` and an `outcome` of `minted`,
`mint_ambiguous` (with the submitted `extrinsic_hash`), `mint_rejected` or
`mint_not_submitted`. If startup fails, the reason is shipped as an `outcome=fatal` record
(`faucet stopped: …`), not only printed to the console. Neither record ever contains the
key.

⚠ An unknown-outcome mint at startup ends the process, and Flux restarts it within seconds,
so "reconcile before restarting" cannot wait for you. Look up the transaction named in the
`faucet mint failed` record. The restarted faucet reads the base balance again: a first mint
that already sits in a block stops a second one, but one still waiting in a node's pool does
not, and both can land (test tokens only; the budget is 1e9 AGLS).

The spec sets `--max-amount-plancks=11000000000000`: the per-request cap defaults to one
dispense (10 AGLS), and the 11 AGLS drips above and in `quip-testnet-health` must get
through it.

There is **no rollback image on 119**: every earlier image mints through
`Sudo::sudo(FaucetOps.mint)`, which runtime 119 rejects. Every later runtime upgrade needs a
rebuilt faucet deployed with it, because the exact 119/7 runtime check answers 503 from the
upgrade block until the new image is live.

## Rollback

Historical. The Akash lease (DSEQ 27496807) was closed on 2026-08-13, so there is no
host-level rollback; on runtime 119 there is no image rollback either (see above). The
original note: while the lease was open, rollback was a DNS revert: drop the CNAME,
restore `A 184.105.162.182`, TTL 60.

## Constraints worth knowing before editing the spec

- `domains.length` must equal `ports.length`, or registration fails.
- App and component names: `a-zA-Z0-9` only, and must not start with `flux` or `zel`.
- Max 20 env vars and 20 commands per component (400 chars each); `containerData` ≤ 200
  chars; `repotag` ≤ 200.
- Port 8087 is legal: Flux allows 1–65535 minus its banned set (`16100-16299`, `26100-26299`,
  `30000-30099`, 8384, 27017, 22, 23, 25, …). Ports `0-1023`, 8080, 8081, 8443 and 6667 are
  Enterprise-only.
- **Do not point `containerData` at `/etc/ssl`.** The volume can shadow the CA bundle, and the
  faucet's outbound WSS to the bootnodes needs those roots. `/data` is unused by the faucet
  (it is stateless) and unreplicated — no `r:` prefix, since there is one instance and no
  state to share.
- Env values may not be empty, and changing one later needs a **Soft Reinstall**; Restart is
  not enough.
- Enterprise apps return `compose: []` to the API, so cpu/ram/hdd/ports are not publicly
  readable afterwards. The dashboard's spec export is canonical — keep it next to this file.
