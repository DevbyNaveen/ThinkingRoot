import http from 'k6/http';
import { check, sleep } from 'k6';
import { Trend, Rate } from 'k6/metrics';

const mcpLatency = new Trend('mcp_latency', true);
const mcpFailRate = new Rate('mcp_fail_rate');

const BASE_URL = __ENV.BASE_URL || 'http://127.0.0.1:9876';
const WS = __ENV.WORKSPACE || 'bench-workspace';

const SEARCH_QUERIES = [
  'authentication flow',
  'database connection',
  'cache invalidation',
  'error handling',
  'configuration loading',
];

let _rpcId = 1;

function makeJsonRpc(method, params) {
  return JSON.stringify({
    jsonrpc: '2.0',
    id: _rpcId++,
    method,
    params,
  });
}

export const options = {
  stages: [
    { duration: '20s', target: 30 },
    { duration: '60s', target: 30 },
    { duration: '20s', target: 100 },
    { duration: '20s', target: 0 },
  ],
  thresholds: {
    'mcp_latency': ['p(95)<15', 'p(99)<30'],
    'mcp_fail_rate': ['rate<0.001'],
  },
};

export default function () {
  const roll = Math.random();
  let body;

  if (roll < 0.80) {
    // 80% — search tool
    const q = SEARCH_QUERIES[Math.floor(Math.random() * SEARCH_QUERIES.length)];
    body = makeJsonRpc('tools/call', {
      name: 'search',
      arguments: { workspace: WS, query: q, top_k: 10 },
    });
  } else if (roll < 0.95) {
    // 15% — query_claims
    body = makeJsonRpc('tools/call', {
      name: 'query_claims',
      arguments: { workspace: WS, claim_type: 'Fact', limit: 20 },
    });
  } else {
    // 5% — health_check
    body = makeJsonRpc('tools/call', {
      name: 'health_check',
      arguments: { workspace: WS },
    });
  }

  const url = `${BASE_URL}/mcp?sessionId=bench`;
  const params = { headers: { 'Content-Type': 'application/json' } };

  const start = Date.now();
  const res = http.post(url, body, params);
  const elapsed = Date.now() - start;

  mcpLatency.add(elapsed);

  const success = check(res, {
    'status 200 or 202': (r) => r.status === 200 || r.status === 202,
    'no rpc error': (r) => {
      try {
        const parsed = JSON.parse(r.body);
        return parsed.error === undefined || parsed.error === null;
      } catch (_) {
        // 202 responses may have empty body
        return r.status === 202;
      }
    },
  });

  mcpFailRate.add(!success);

  sleep(0.1);
}
