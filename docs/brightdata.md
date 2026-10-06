# Bright Data setup

Bright Data is optional. Search and fetch work without an account or key.
SERP API serves searches; Web Unlocker is the paid fallback when local fetch
and the browser cannot get through a bot wall. They require separate zones.

## Create the zones and key

1. Open [My Zones](https://brightdata.com/cp/zones) in your Bright Data account.
2. Create a **SERP API** zone for search. Copy its exact name.
3. Create a **Web Unlocker API** zone for fetch. Copy its exact name.
4. Get an **API key** from [account settings](https://brightdata.com/cp/setting/users).
   Use the API token, not the proxy username/password or customer ID. The
   token needs access to both zones if you use both products.

The names `serp_api1` and `web_unlocker1` are defaults in DonSeTch, not zones
it creates for you. If your zone has another name, include it after `::`.

## Add the keys

Replace the placeholders, keeping the quotes:

```sh
donsetch keys add brightdata "YOUR_API_TOKEN::YOUR_SERP_ZONE"
donsetch keys add unlocker "YOUR_API_TOKEN::YOUR_UNLOCKER_ZONE"
donsetch keys list
donsetch doctor --deep
```

For example, a SERP zone named `serp_donsetch_laptop` needs
`"YOUR_API_TOKEN::serp_donsetch_laptop"`. A Web Unlocker zone goes under
`unlocker`, never under `brightdata`. Restart your MCP server after adding
search keys. Keys are stored locally; do not paste them into an issue.

**HTML or JSON?** DonSeTch chooses the response format on each request:
search asks for parsed Google JSON using `brd_json=1` with `format: "raw"`;
Unlocker asks for a JSON envelope containing the target status, headers
and HTML body. You do not need to edit a request body or convert the output.
The dashboard's code example format choice is not something to copy into
DonSeTch configuration. Keep the Unlocker output as HTML rather than a
markdown/screenshot transformation so DonSeTch can extract it normally.

## Try it

```sh
donsetch keys default brightdata
donsetch search "Rust async programming"
donsetch fetch "https://example.com"
```

Search uses the configured provider chain. To prefer keyless search while
keeping the paid search providers as fallback, run `donsetch keys default local`.
Unlocker is fetch-only: adding it does not change the search default.
The example fetch normally succeeds locally and spends no Unlocker credit.
Unlocker runs automatically on the wall fallback; `--tier 1` prevents escalation.
There is no `--tier 3` command.

## Costs and limits

Unlocker defaults to 50 API attempts per UTC day, a 120-second timeout,
and a sliding six-hour local cache with up to 200 entries. Cache hits do
not call Bright Data or consume the daily cap. A confirmed transient solve
failure may be retried once; each API attempt consumes a cap unit. A timed
out paid request is not automatically replayed because its billing outcome
is unknown. Check the dashboard for actual charges and zone pricing.

```sh
donsetch config show
donsetch config set bypass.enabled false
donsetch config set bypass.max_daily 20
donsetch config set bypass.render true
```

Rendering can help pages that need JavaScript and can increase latency.
`doctor --deep` makes a free account-zone check for configured Bright Data
zones, verifying token access, the exact zone name and product type. If
the token cannot list account zones, the check reports that it was skipped.
The check does not prove a particular target can be unlocked.

## Troubleshooting

| Message | Action |
| --- | --- |
| `zone "serp_api1" not found` | Use your actual SERP zone after `::`; the default does not exist in every account. |
| Wrong zone/product or HTTP 403 | Check product type, token permissions and account policy. A SERP zone cannot unlock arbitrary pages. |
| Token rejected / HTTP 401 | Replace the API token, then restart the MCP server. |
| No balance / HTTP 402 | Check billing and zone access, then reset the parked key. |
| HTTP 429 | Wait for the 60-second cooldown; the key becomes eligible automatically. |
| SERP timeout / empty response | The failure stays visible and search tries the remaining provider chain, then keyless search. Check Bright Data's request log; the token is not permanently disabled. |
| Daily cap reached | Wait for the UTC reset or deliberately raise `bypass.max_daily`. |
| Counter cannot be read/written | Fix the cache directory permissions or corrupt counter before another paid call. |
| Target still walled | Check that the site is supported by the zone; an HTTP 200 challenge is still a failed fetch. |

After fixing a rejected/depleted key:

```sh
donsetch keys reset brightdata
donsetch keys reset unlocker
donsetch doctor --deep
```

See Bright Data's [SERP API introduction](https://docs.brightdata.com/scraping-automation/serp-api/introduction)
and [Web Unlocker request reference](https://docs.brightdata.com/api-reference/rest-api/unlocker/unlock-website).
