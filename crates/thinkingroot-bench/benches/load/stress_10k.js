// ─── ThinkingRoot 10,000 VU Stress Test ──────────────────────────────────────
// Purpose : Prove ThinkingRoot REST API holds sub-10ms p95 at 10K concurrent users.
// Endpoint: /api/v1/ws/{workspace}/entities  (pure in-memory HashMap read — fastest path)
// Ramp    : 5-stage ramp to 10,000 VUs, 5-minute sustain at peak, then drain.
// ─────────────────────────────────────────────────────────────────────────────

import http from 'k6/http';
import { check, sleep } from 'k6';
import { Trend, Rate, Counter } from 'k6/metrics';

// ── Custom metrics ────────────────────────────────────────────────────────────
const entityLatency  = new Trend('entity_latency',  true);   // ms
const searchLatency  = new Trend('search_latency',  true);   // ms
const failRate       = new Rate('fail_rate');
const totalRequests  = new Counter('total_requests');

// ── Config ────────────────────────────────────────────────────────────────────
const BASE_URL  = __ENV.BASE_URL  || 'http://127.0.0.1:9876';
const WORKSPACE = __ENV.WORKSPACE || 'stress-workspace';

const SEARCH_TERMS = [
  'authentication',  'database',  'cache',  'config',
  'error',           'handler',   'module', 'service',
  'request',         'response',
];

// ── Load profile ──────────────────────────────────────────────────────────────
export const options = {
  stages: [
    { duration: '1m',  target: 500   },   // Warm-up: ramp to 500
    { duration: '2m',  target: 2000  },   // Build:   ramp to 2K
    { duration: '3m',  target: 5000  },   // Climb:   ramp to 5K
    { duration: '3m',  target: 10000 },   // Peak:    ramp to 10K
    { duration: '5m',  target: 10000 },   // Sustain: hold 10K for 5 minutes
    { duration: '2m',  target: 0     },   // Drain
  ],

  thresholds: {
    // ── Performance gates ──
    'entity_latency':          ['p(95)<10', 'p(99)<25'],
    'search_latency':          ['p(95)<10', 'p(99)<25'],
    'fail_rate':               ['rate<0.01'],   // < 1% error rate
    'http_req_duration':       ['p(95)<15'],
    'http_req_failed':         ['rate<0.01'],

    // ── Throughput floor ──
    'http_reqs':               ['rate>1000'],   // > 1000 req/s throughout
  },

  // keep-alive ON by default in k6 — connections reused across iterations
  userAgent: 'ThinkingRoot-StressTest/1.0',
};

// ── VU scenario ───────────────────────────────────────────────────────────────
export default function () {
  const roll = Math.random();

  if (roll < 0.70) {
    // 70% — entity list (fastest path: single HashMap scan)
    const url    = `${BASE_URL}/api/v1/ws/${WORKSPACE}/entities`;
    const params = { headers: { 'Accept': 'application/json' } };

    const res = http.get(url, params);

    entityLatency.add(res.timings.duration);
    totalRequests.add(1);

    const ok = check(res, {
      'entities 200': (r) => r.status === 200,
      'ok flag':      (r) => { try { return JSON.parse(r.body).ok === true; } catch (_) { return false; } },
    });
    failRate.add(!ok);

  } else if (roll < 0.95) {
    // 25% — vector search (requires fastembed — measures full search path)
    const term   = SEARCH_TERMS[Math.floor(Math.random() * SEARCH_TERMS.length)];
    const url    = `${BASE_URL}/api/v1/ws/${WORKSPACE}/search?q=${encodeURIComponent(term)}&top_k=5`;
    const params = { headers: { 'Accept': 'application/json' } };

    const res = http.get(url, params);

    searchLatency.add(res.timings.duration);
    totalRequests.add(1);

    const ok = check(res, {
      'search 200': (r) => r.status === 200,
      'ok flag':    (r) => { try { return JSON.parse(r.body).ok === true; } catch (_) { return false; } },
    });
    failRate.add(!ok);

  } else {
    // 5% — health check (minimal overhead, control signal)
    const url = `${BASE_URL}/api/v1/ws/${WORKSPACE}/health`;
    const res = http.get(url);
    totalRequests.add(1);
    const ok = check(res, { 'health 200': (r) => r.status === 200 });
    failRate.add(!ok);
  }

  // No sleep — maximum pressure test
}
