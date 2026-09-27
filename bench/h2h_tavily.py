#!/usr/bin/env python3
"""
Head-to-head: DonSeTch keyless vs Tavily BYOK
Realistic agent queries, not trivia. Broad across domains and difficulty.
"""

import json
import subprocess
import os
import time
import sys

ENV = {**os.environ, "PATH": os.path.expanduser("~/.npm-global/bin:") + os.environ.get("PATH", "")}

# 40 realistic queries an agent would actually send
# Mix of: factual, research, troubleshooting, current events, code, niche
QUERIES = [
    # Factual lookup (easy baseline)
    "what is the TCP three-way handshake",
    "how does BPF work in Linux",
    "what is RAFT consensus algorithm",
    "explain CRDT data structures",
    "what is the CAP theorem in distributed systems",

    # Real research (medium)
    "how to implement OAuth2 device flow",
    "best practices for PostgreSQL index optimization",
    "how to handle websocket reconnection logic",
    "sqlite WAL mode vs journal mode performance",
    "how does eBPF packet filtering compare to iptables",

    # Troubleshooting / debugging (harder)
    "rust tokio task panic not caught spawn blocking",
    "nginx 502 bad gateway upstream timeout fix",
    "docker container OOM killed but memory limit not reached",
    "kubernetes pod crashloopbackoff troubleshoot",
    "git rebase conflict abort vs continue",

    # Code / dev (practical)
    "python asyncio gather vs wait difference",
    "typescript discriminated unions type narrowing",
    "go context timeout propagation best practice",
    "react useeffect cleanup function when to use",
    "postgres jsonb vs json column type difference",

    # Current / trending (tests freshness)
    "latest LLM benchmarks 2025 comparison",
    "Rust 1.98 release notes new features",
    "new JavaScript features ES2025",
    "Python 3.14 what changed",
    "Claude vs GPT-5 vs Gemini comparison 2025",

    # Niche / obscure (hard)
    "how to implement B-tree deletion with rebalancing",
    "what is the difference between MPC and FHE in cryptography",
    "how does SPDY differ from HTTP/2",
    "explain the Paxos algorithm simply",
    "what is structural subtyping in OCaml",

    # Real-world entity / current (tests if results are fresh and specific)
    "what is the latest version of Deno",
    "what is the current LTS version of Node.js",
    "who is the CTO of Cloudflare",
    "what programming language is Discord written in",

    # Multi-intent (agent might ask these)
    "how to set up CI with GitHub Actions for Rust project",
    "compare REST vs GraphQL vs gRPC for microservices",
    "what are the security risks of using websockets",
    "how to migrate from MySQL to PostgreSQL step by step",
    "best tool for web scraping in 2025 Python vs Rust",

    # Long-tail (realistic agent phrasing)
    "why does my Rust borrow checker complain about moving a value into a closure",
    "how to debug memory leak in Node.js production application",
    "what is the fastest way to parse JSON in Python for large files",
    "is AGPL v3 compatible with commercial use",
    "how does Cloudflare bot detection work technically",
]

def run_donsetch_local(query, max_results=10):
    """Run donsetch search with local keyless engine"""
    cmd = ["donsetch", "search", query, "--max-results", str(max_results), "--json"]
    start = time.time()
    try:
        result = subprocess.run(cmd, capture_output=True, text=True, timeout=30, env=ENV)
        latency = (time.time() - start) * 1000
        data = json.loads(result.stdout)
        results = data.get("meta", {}).get("results", [])
        elapsed = data.get("meta", {}).get("elapsed_ms", latency)
        return results, elapsed
    except Exception as e:
        return [], (time.time() - start) * 1000

def run_donsetch_tavily(query, max_results=10):
    """Run donsetch search with Tavily BYOK"""
    # Set default to tavily for this call
    env = {**ENV, "DONSETCH_SEARCH_DEFAULT": "tavily"}
    cmd = ["donsetch", "search", query, "--max-results", str(max_results), "--json"]
    start = time.time()
    try:
        result = subprocess.run(cmd, capture_output=True, text=True, timeout=30, env=env)
        latency = (time.time() - start) * 1000
        data = json.loads(result.stdout)
        results = data.get("meta", {}).get("results", [])
        elapsed = data.get("meta", {}).get("elapsed_ms", latency)
        return results, elapsed
    except Exception as e:
        return [], (time.time() - start) * 1000

def extract_domain(url):
    """Extract domain from URL"""
    try:
        from urllib.parse import urlparse
        p = urlparse(url)
        domain = p.netloc.lower()
        if domain.startswith("www."):
            domain = domain[4:]
        return domain
    except:
        return ""

def domain_overlap(results_a, results_b):
    """How many domains appear in both result sets"""
    domains_a = set(extract_domain(r.get("url", "")) for r in results_a)
    domains_b = set(extract_domain(r.get("url", "")) for r in results_b)
    overlap = domains_a & domains_b
    return len(overlap), len(domains_a), len(domains_b)

def url_overlap(results_a, results_b):
    """How many exact URLs appear in both result sets"""
    urls_a = set(r.get("url", "") for r in results_a)
    urls_b = set(r.get("url", "") for r in results_b)
    overlap = urls_a & urls_b
    return len(overlap), len(urls_a), len(urls_b)

def main():
    print(f"Head-to-head: DonSeTch keyless vs Tavily")
    print(f"Queries: {len(QUERIES)}")
    print(f"{'='*100}")

    all_results = []

    for i, query in enumerate(QUERIES):
        print(f"\n[{i+1}/{len(QUERIES)}] {query[:70]}")

        # Run local first
        local_results, local_latency = run_donsetch_local(query)
        time.sleep(0.5)

        # Run tavily
        tavily_results, tavily_latency = run_donsetch_tavily(query)
        time.sleep(0.5)

        # Compare
        d_overlap, d_local, d_tavily = domain_overlap(local_results, tavily_results)
        u_overlap, u_local, u_tavily = url_overlap(local_results, tavily_results)

        # Top result comparison
        local_top = local_results[0] if local_results else None
        tavily_top = tavily_results[0] if tavily_results else None

        local_top_domain = extract_domain(local_top["url"]) if local_top else "none"
        tavily_top_domain = extract_domain(tavily_top["url"]) if tavily_top else "none"

        local_top_title = local_top["title"][:60] if local_top else "none"
        tavily_top_title = tavily_top["title"][:60] if tavily_top else "none"

        print(f"  LOCAL:  {len(local_results):2d} results  {local_latency:5.0f}ms  top: {local_top_domain:25s} {local_top_title}")
        print(f"  TAVILY: {len(tavily_results):2d} results  {tavily_latency:5.0f}ms  top: {tavily_top_domain:25s} {tavily_top_title}")
        print(f"  URL overlap: {u_overlap}  Domain overlap: {d_overlap}")

        # Same top domain?
        same_top = local_top_domain == tavily_top_domain if local_top and tavily_top else False
        marker = "==" if same_top else "!="
        print(f"  Top match: {marker}")

        all_results.append({
            "query": query,
            "local_count": len(local_results),
            "tavily_count": len(tavily_results),
            "local_latency": local_latency,
            "tavily_latency": tavily_latency,
            "url_overlap": u_overlap,
            "domain_overlap": d_overlap,
            "same_top_domain": same_top,
            "local_top": {"title": local_top_title, "domain": local_top_domain, "url": local_top["url"] if local_top else ""},
            "tavily_top": {"title": tavily_top_title, "domain": tavily_top_domain, "url": tavily_top["url"] if tavily_top else ""},
            "local_results": [{"title": r.get("title",""), "url": r.get("url",""), "snippet": r.get("snippet","")[:200]} for r in local_results[:5]],
            "tavily_results": [{"title": r.get("title",""), "url": r.get("url",""), "snippet": r.get("snippet","")[:200]} for r in tavily_results[:5]],
        })

    # Summary
    print(f"\n{'='*100}")
    print(f"SUMMARY")
    print(f"{'='*100}")

    total = len(all_results)
    same_top = sum(1 for r in all_results if r["same_top_domain"])
    local_faster = sum(1 for r in all_results if r["local_latency"] < r["tavily_latency"])
    avg_local_lat = sum(r["local_latency"] for r in all_results) / total
    avg_tavily_lat = sum(r["tavily_latency"] for r in all_results) / total
    avg_url_overlap = sum(r["url_overlap"] for r in all_results) / total
    avg_domain_overlap = sum(r["domain_overlap"] for r in all_results) / total
    avg_local_count = sum(r["local_count"] for r in all_results) / total
    avg_tavily_count = sum(r["tavily_count"] for r in all_results) / total

    print(f"  Queries:                     {total}")
    print(f"  Same top domain:              {same_top}/{total} ({same_top/total:.0%})")
    print(f"  Local faster:                 {local_faster}/{total}")
    print(f"  Avg local latency:            {avg_local_lat:.0f}ms")
    print(f"  Avg Tavily latency:          {avg_tavily_lat:.0f}ms")
    print(f"  Avg results (local):         {avg_local_count:.1f}")
    print(f"  Avg results (Tavily):        {avg_tavily_count:.1f}")
    print(f"  Avg URL overlap:             {avg_url_overlap:.1f}")
    print(f"  Avg domain overlap:          {avg_domain_overlap:.1f}")

    # Where they disagree
    print(f"\n  DISAGREEMENTS (different top domain):")
    for r in all_results:
        if not r["same_top_domain"]:
            print(f"    Q: {r['query'][:60]}")
            print(f"      L: {r['local_top']['domain']:25s} {r['local_top']['title'][:50]}")
            print(f"      T: {r['tavily_top']['domain']:25s} {r['tavily_top']['title'][:50]}")

    # Save
    out_path = os.path.expanduser("~/.cache/donsetch/h2h_results.json")
    os.makedirs(os.path.dirname(out_path), exist_ok=True)
    with open(out_path, "w") as f:
        json.dump(all_results, f, indent=2)
    print(f"\n  Saved to {out_path}")

if __name__ == "__main__":
    main()
