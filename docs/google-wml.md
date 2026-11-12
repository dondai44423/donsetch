# Native Google search

The 5.3 candidate's default Google source is `google-cse`: the public search-element
bootstrap and structured JSONP interface, read through DonSeTch's native HTTP transport.
Search requires no user API key, account, browser, script execution or model download.
Availability depends on Google and the network; a successful witness establishes the
request contract, not sustained availability.

## Request and refusal handling

The bootstrap supplies our own short-lived token and library version. Its cache lasts
at most one hour and is separated by binary version, host platform and effective route.
Bootstrap and result requests use the same egress. A cold call normally needs two HTTP
exchanges and a warm token one; redirects count toward the shared physical-request limit.

Responses are bounded before parsing. An unknown schema is a source error, and a quota
response is a rest rather than an empty successful result. Published waits keep their
full duration. Search does not change identity, endpoint or egress to evade a refusal.
The mobile endpoint is not a fallback for a refused search-element request.

The source enters the normal admission, ranking, measured-family and confidence pipeline.
A distinct endpoint name does not by itself prove an independent index. Only high-confidence
merged answers are cached; source caches retain their own grade and age.

## Legacy mobile research

`src/search/engines/google_wml.rs` and the `google_wml_probe` example retain the old
mobile protocol for research and historical fixtures. They are outside the default
search roster. `search.google_profile` and the legacy `DONSETCH_GOOGLE_PROFILE` select
that helper's initial profile; they do not switch production search away from CSE.
Historical `google_http_v1` health and cached profile IDs are not CSE preferences.

Earlier small samples accepted some mobile profiles. Those samples did not establish
sustained availability or justify identity/endpoint rotation after a rate limit.

## Verification

`just t cse` checks bootstrap parsing, structured result/error handling, ranks and locale
parameters. The search suite checks shared request ceilings, cooldown persistence, source
families and cache eligibility. Live acceptance and quality remain separate measurements
in the release's search comparison artifacts.

Protocol references, not runtime dependencies:
[Google's search-element documentation](https://developers.google.com/custom-search/docs/element),
[SearXNG's native CSE adapter](https://github.com/searxng/searxng/blob/master/searx/engines/google_cse.py).
