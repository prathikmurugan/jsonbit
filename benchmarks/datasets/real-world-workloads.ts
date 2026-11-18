/**
 * Real-World Benchmark Workloads & Ground-Truth Query Suite for `json-olap` (Pure-Rust Engine)
 *
 * Covers 10 distinct production topologies, including edge cases:
 *  - W1: Tiny 4-row REST API response (tests Adaptive Micro-Card overhead)
 *  - W2: Deep Singleton Kubernetes / Service Config (tests singleton config tree handling)
 *  - W3: Flat HR & Payroll Ledger (200 rows; tests O(1) headers, GROUP BY + ORDER BY + LIMIT)
 *  - W4: 1-to-Many E-Commerce Orders x Line Items (150 orders, ~450 items; tests CSR parent-child JOINs)
 *  - W5: Sparse OpenTelemetry Production NDJSON Logs (300 rows; 80% sparse error.* + free-text LIKE '%timeout%')
 *  - W6: GitHub Repository Telemetry (200 repos; high-cardinality names + integer ranges)
 *  - W7: Stripe Payment & Dispute Webhooks (200 events; nested dot-paths + boolean bitmaps)
 *  - W8: IAM Policies with Primitive Arrays (150 users; primitive array membership & _count columns)
 *  - W9: Wide Enterprise CRM Export (150 rows x 30 columns; wide schema scaling)
 *  - W10: Adversarial Unicode & Numeric Ledger (150 rows; UTF-8/emoji, .00 whole floats, explicit nulls)
 */

export interface BenchmarkQuestion {
  id: string
  workload: string
  category:
    | 'point-lookup'
    | 'o1-header-agg'
    | 'bitmap-filter'
    | 'multi-clause-range'
    | 'null-negation'
    | 'text-search-like'
    | 'group-by-top-k'
    | 'csr-child-join'
    | 'primitive-array'
    | 'dot-path-nested'
  prompt: string
  expectedSql: string
  groundTruth: string
  answerType: 'integer' | 'float' | 'string' | 'boolean'
}

export interface RealWorldWorkload {
  id: string
  title: string
  category: string
  format: 'json' | 'jsonl'
  description: string
  data: unknown
  rawText: string
  questions: BenchmarkQuestion[]
}

// Deterministic PRNG (Mulberry32) so benchmarks are 100% reproducible
function createRng(seed: number): () => number {
  let s = seed >>> 0
  return () => {
    s = (s + 0x6d2b79f5) >>> 0
    let t = Math.imul(s ^ (s >>> 15), 1 | s)
    t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296
  }
}

export function buildRealWorldWorkloads(): RealWorldWorkload[] {
  const workloads: RealWorldWorkload[] = []

  // ============================================================================
  // W1: Tiny 4-Row REST API Payload (Counter-Case / Crossover Test)
  // ============================================================================
  const w1Data = {
    nodes: [
      { nodeId: 'n-01', zone: 'us-east-1a', cpuPct: 42, healthy: true },
      { nodeId: 'n-02', zone: 'us-east-1b', cpuPct: 88, healthy: true },
      { nodeId: 'n-03', zone: 'eu-west-1a', cpuPct: 95, healthy: false },
      { nodeId: 'n-04', zone: 'ap-south-1a', cpuPct: 31, healthy: true },
    ],
  }
  workloads.push({
    id: 'W1-tiny-api-response',
    title: 'Tiny 4-Row Cluster Status API',
    category: 'Edge Case: Tiny Payload (<500B)',
    format: 'json',
    description: '4-row microservice health response testing Adaptive Micro-Card overhead.',
    data: w1Data,
    rawText: JSON.stringify(w1Data, null, 2),
    questions: [
      {
        id: 'w1-q1',
        workload: 'W1-tiny-api-response',
        category: 'bitmap-filter',
        prompt: 'How many nodes are healthy?',
        expectedSql: `SELECT COUNT(*) AS cnt FROM nodes WHERE healthy = true`,
        groundTruth: '3',
        answerType: 'integer',
      },
      {
        id: 'w1-q2',
        workload: 'W1-tiny-api-response',
        category: 'point-lookup',
        prompt: 'What is the zone of node n-03?',
        expectedSql: `SELECT zone FROM nodes WHERE nodeId = 'n-03'`,
        groundTruth: 'eu-west-1a',
        answerType: 'string',
      },
      {
        id: 'w1-q3',
        workload: 'W1-tiny-api-response',
        category: 'o1-header-agg',
        prompt: 'What is the maximum cpuPct across all nodes?',
        expectedSql: `SELECT MAX(cpuPct) AS mx FROM nodes`,
        groundTruth: '95',
        answerType: 'integer',
      },
    ],
  })

  // ============================================================================
  // W2: Deep Singleton Kubernetes / Service Config
  // ============================================================================
  const w2Data = {
    cluster: {
      name: 'prod-us-east-eks',
      version: '1.31',
      network: {
        vpcId: 'vpc-0a9b8c7d',
        cidr: '10.24.0.0/16',
        natGateways: 3,
        privateSubnets: true,
      },
      autoscaler: {
        enabled: true,
        minNodes: 6,
        maxNodes: 64,
        scaleDownDelaySec: 300,
      },
      observability: {
        prometheusPort: 9090,
        tracingSampleRate: 0.25,
        logRetentionDays: 30,
      },
    },
  }
  workloads.push({
    id: 'W2-singleton-k8s-config',
    title: 'Singleton Kubernetes Cluster Config',
    category: 'Edge Case: Deep Singleton Config',
    format: 'json',
    description: 'Deeply nested singleton configuration object with 0 repeating row arrays.',
    data: w2Data,
    rawText: JSON.stringify(w2Data, null, 2),
    questions: [
      {
        id: 'w2-q1',
        workload: 'W2-singleton-k8s-config',
        category: 'dot-path-nested',
        prompt: 'What is the autoscaler maxNodes setting in the cluster config?',
        expectedSql: `SELECT cluster_autoscaler_maxNodes FROM config`,
        groundTruth: '64',
        answerType: 'integer',
      },
      {
        id: 'w2-q2',
        workload: 'W2-singleton-k8s-config',
        category: 'dot-path-nested',
        prompt: 'What is the network cidr of the cluster?',
        expectedSql: `SELECT cluster_network_cidr FROM config`,
        groundTruth: '10.24.0.0/16',
        answerType: 'string',
      },
      {
        id: 'w2-q3',
        workload: 'W2-singleton-k8s-config',
        category: 'dot-path-nested',
        prompt: 'What is the observability prometheusPort?',
        expectedSql: `SELECT cluster_observability_prometheusPort FROM config`,
        groundTruth: '9090',
        answerType: 'integer',
      },
    ],
  })

  // ============================================================================
  // W3: Flat HR & Payroll Directory (200 rows)
  // ============================================================================
  const rng3 = createRng(1003)
  const depts = ['Engineering', 'Sales', 'Product', 'Security', 'Operations', 'Finance']
  const levels = ['L3', 'L4', 'L5', 'L6', 'L7']
  const offices = ['San Francisco', 'New York', 'London', 'Bangalore', 'Remote']
  const employees = Array.from({ length: 200 }, (_, i) => {
    const dept = depts[Math.floor(rng3() * depts.length)]!
    const level = levels[Math.floor(rng3() * levels.length)]!
    const office = offices[Math.floor(rng3() * offices.length)]!
    const salary = 90000 + Math.floor(rng3() * 160) * 1000
    const bonusPct = Math.round((5 + rng3() * 20) * 10) / 10
    const remote = office === 'Remote' || rng3() > 0.65
    return {
      empId: `EMP-${String(i + 1001)}`,
      name: `Employee_${i + 1}`,
      department: dept,
      level,
      office,
      salary,
      bonusPct,
      remote,
    }
  })
  const w3Data = { employees }
  const engRemoteCnt = employees.filter(
    e => e.department === 'Engineering' && e.remote && e.salary > 150000,
  ).length
  const totalPayroll = employees.reduce((acc, e) => acc + e.salary, 0)
  const secCount = employees.filter(e => e.department === 'Security').length

  workloads.push({
    id: 'W3-hr-payroll-directory',
    title: 'HR & Payroll Directory (200 rows)',
    category: 'Tabular Analytical',
    format: 'json',
    description: '200 employee records with categorical enums, salaries, floats, and booleans.',
    data: w3Data,
    rawText: JSON.stringify(w3Data, null, 2),
    questions: [
      {
        id: 'w3-q1',
        workload: 'W3-hr-payroll-directory',
        category: 'o1-header-agg',
        prompt: 'What is the total salary across all employees?',
        expectedSql: `SELECT SUM(salary) AS total_salary FROM employees`,
        groundTruth: String(totalPayroll),
        answerType: 'integer',
      },
      {
        id: 'w3-q2',
        workload: 'W3-hr-payroll-directory',
        category: 'multi-clause-range',
        prompt: 'How many remote Engineering employees have a salary greater than 150000?',
        expectedSql: `SELECT COUNT(*) AS cnt FROM employees WHERE department = 'Engineering' AND remote = true AND salary > 150000`,
        groundTruth: String(engRemoteCnt),
        answerType: 'integer',
      },
      {
        id: 'w3-q3',
        workload: 'W3-hr-payroll-directory',
        category: 'bitmap-filter',
        prompt: 'How many employees are in the Security department?',
        expectedSql: `SELECT COUNT(*) AS cnt FROM employees WHERE department = 'Security'`,
        groundTruth: String(secCount),
        answerType: 'integer',
      },
      {
        id: 'w3-q4',
        workload: 'W3-hr-payroll-directory',
        category: 'point-lookup',
        prompt: `What is the salary of employee EMP-1050?`,
        expectedSql: `SELECT salary FROM employees WHERE empId = 'EMP-1050'`,
        groundTruth: String(employees[49]!.salary),
        answerType: 'integer',
      },
    ],
  })

  // ============================================================================
  // W4: 1-to-Many E-Commerce Orders x Line Items (150 orders, ~450 child items)
  // ============================================================================
  const rng4 = createRng(2004)
  const statuses = ['delivered', 'shipped', 'processing', 'cancelled', 'refunded']
  const tiers = ['enterprise', 'pro', 'starter']
  const catalog = [
    { sku: 'SKU-GPU-01', name: 'H100 Compute Node', price: 2400.0 },
    { sku: 'SKU-NVME-02', name: 'NVMe Raid Array', price: 450.5 },
    { sku: 'SKU-NIC-03', name: 'Infiniband Switch', price: 890.0 },
    { sku: 'SKU-PSU-04', name: 'Redundant Power Supply', price: 180.25 },
    { sku: 'SKU-RACK-05', name: 'Server Rack 42U', price: 1150.0 },
  ]
  const orders = Array.from({ length: 150 }, (_, i) => {
    const status = statuses[Math.floor(rng4() * statuses.length)]!
    const tier = tiers[Math.floor(rng4() * tiers.length)]!
    const itemCount = 1 + Math.floor(rng4() * 4)
    const items = []
    let total = 0
    for (let j = 0; j < itemCount; j++) {
      const prod = catalog[(i + j * 2) % catalog.length]!
      const quantity = 1 + Math.floor(rng4() * 5)
      items.push({
        sku: prod.sku,
        name: prod.name,
        quantity,
        unitPrice: prod.price,
      })
      total += quantity * prod.price
    }
    return {
      orderId: `ORD-${String(5001 + i)}`,
      customer: {
        customerId: `CUST-${100 + (i % 60)}`,
        tier,
        region: ['us-east', 'eu-central', 'ap-northeast'][i % 3]!,
      },
      status,
      items,
      orderTotal: Math.round(total * 100) / 100,
    }
  })
  const w4Data = { orders }
  const h100TotalQty = orders.reduce(
    (sum, o) =>
      sum +
      o.items
        .filter(it => it.name === 'H100 Compute Node')
        .reduce((s, it) => s + it.quantity, 0),
    0,
  )
  const deliveredSwitchOrders = orders.filter(
    o => o.status === 'delivered' && o.items.some(it => it.name === 'Infiniband Switch'),
  ).length
  const entEuDeliveredCnt = orders.filter(
    o =>
      o.customer.tier === 'enterprise' &&
      o.customer.region === 'eu-central' &&
      o.status === 'delivered',
  ).length

  workloads.push({
    id: 'W4-ecommerce-orders-1toN',
    title: 'E-Commerce Orders x Line Items (150 orders / ~380 items)',
    category: '1-to-Many Relational Hierarchy',
    format: 'json',
    description: 'Nested orders with customer sub-objects and 1-to-Many line item arrays.',
    data: w4Data,
    rawText: JSON.stringify(w4Data, null, 2),
    questions: [
      {
        id: 'w4-q1',
        workload: 'W4-ecommerce-orders-1toN',
        category: 'csr-child-join',
        prompt: 'What is the total quantity of H100 Compute Node ordered across all orders?',
        expectedSql: `SELECT SUM(quantity) AS total_qty FROM orders_items WHERE name = 'H100 Compute Node'`,
        groundTruth: String(h100TotalQty),
        answerType: 'integer',
      },
      {
        id: 'w4-q2',
        workload: 'W4-ecommerce-orders-1toN',
        category: 'csr-child-join',
        prompt: 'How many delivered orders include an Infiniband Switch?',
        expectedSql: `SELECT COUNT(DISTINCT orderId) AS cnt FROM orders JOIN orders_items WHERE status = 'delivered' AND name = 'Infiniband Switch'`,
        groundTruth: String(deliveredSwitchOrders),
        answerType: 'integer',
      },
      {
        id: 'w4-q3',
        workload: 'W4-ecommerce-orders-1toN',
        category: 'dot-path-nested',
        prompt: 'How many delivered orders are for enterprise customers in eu-central?',
        expectedSql: `SELECT COUNT(*) AS cnt FROM orders WHERE customer.tier = 'enterprise' AND customer.region = 'eu-central' AND status = 'delivered'`,
        groundTruth: String(entEuDeliveredCnt),
        answerType: 'integer',
      },
    ],
  })

  // ============================================================================
  // W5: Sparse OpenTelemetry Production NDJSON Logs (300 rows, .jsonl, free-text)
  // ============================================================================
  const rng5 = createRng(3005)
  const services = ['auth-service', 'checkout-api', 'inventory-worker', 'payment-gateway']
  const okMessages = [
    'Request completed successfully within SLA',
    'Token signature verified via local JWKS cache',
    'Inventory reservation committed to primary shard',
    'Healthcheck probe HTTP 200 OK',
  ]
  const errMessages = [
    'Upstream connection timeout waiting for db-replica-03',
    'TLS handshake timeout connecting to stripe-gateway',
    'Deadlock detected on row lock in inventory_ledger',
    'Rate limit exceeded for tenant api_key',
  ]
  const logRows = Array.from({ length: 300 }, (_, i) => {
    const isErr = rng5() < 0.22
    const service = services[Math.floor(rng5() * services.length)]!
    const statusCode = isErr ? (rng5() < 0.6 ? 503 : 500) : 200
    const durationMs = isErr
      ? 1200 + Math.floor(rng5() * 3800)
      : 12 + Math.floor(rng5() * 140)
    const msg = isErr
      ? errMessages[Math.floor(rng5() * errMessages.length)]!
      : okMessages[Math.floor(rng5() * okMessages.length)]!

    const base: Record<string, unknown> = {
      traceId: `tr-${String(10000 + i)}`,
      service,
      statusCode,
      durationMs,
      message: msg,
    }
    if (isErr) {
      base.error = {
        code: statusCode === 503 ? 'UPSTREAM_UNAVAILABLE' : 'INTERNAL_FAULT',
        retryable: statusCode === 503,
      }
    }
    return base
  })
  const w5Jsonl = logRows.map(r => JSON.stringify(r)).join('\n')
  const timeoutLogsCnt = logRows.filter(r =>
    String(r.message).toLowerCase().includes('timeout'),
  ).length
  const retryableCheckoutCnt = logRows.filter(
    r =>
      r.service === 'checkout-api' &&
      (r.error as { retryable?: boolean } | undefined)?.retryable === true,
  ).length
  const nonNullErrorCnt = logRows.filter(r => r.error !== undefined).length

  workloads.push({
    id: 'W5-otel-production-logs',
    title: 'OpenTelemetry Production NDJSON Logs (300 lines)',
    category: 'Sparse Semi-Structured NDJSON + Free-Text',
    format: 'jsonl',
    description:
      '300 NDJSON trace lines where ~78% omit the error object entirely, with free-text log messages.',
    data: { records: logRows },
    rawText: w5Jsonl,
    questions: [
      {
        id: 'w5-q1',
        workload: 'W5-otel-production-logs',
        category: 'text-search-like',
        prompt: 'How many log records have a message containing the word "timeout"?',
        expectedSql: `SELECT COUNT(*) AS cnt FROM records WHERE message LIKE '%timeout%'`,
        groundTruth: String(timeoutLogsCnt),
        answerType: 'integer',
      },
      {
        id: 'w5-q2',
        workload: 'W5-otel-production-logs',
        category: 'dot-path-nested',
        prompt: 'How many checkout-api logs have a retryable error (error.retryable = true)?',
        expectedSql: `SELECT COUNT(*) AS cnt FROM records WHERE service = 'checkout-api' AND error.retryable = true`,
        groundTruth: String(retryableCheckoutCnt),
        answerType: 'integer',
      },
      {
        id: 'w5-q3',
        workload: 'W5-otel-production-logs',
        category: 'null-negation',
        prompt: 'How many log records have a non-null error.code?',
        expectedSql: `SELECT COUNT(*) AS cnt FROM records WHERE error.code IS NOT NULL`,
        groundTruth: String(nonNullErrorCnt),
        answerType: 'integer',
      },
    ],
  })

  // ============================================================================
  // W6: GitHub Repository Telemetry (200 repos)
  // ============================================================================
  const rng6 = createRng(4006)
  const langs = ['Rust', 'TypeScript', 'Python', 'Go', 'C++']
  const licenses = ['MIT', 'Apache-2.0', 'GPL-3.0', 'BSD-3-Clause']
  const repos = Array.from({ length: 200 }, (_, i) => {
    const language = langs[Math.floor(rng6() * langs.length)]!
    const license = licenses[Math.floor(rng6() * licenses.length)]!
    const stars = 500 + Math.floor(rng6() * 95000)
    const forks = 50 + Math.floor(rng6() * 12000)
    const openIssues = Math.floor(rng6() * 400)
    const archived = rng6() < 0.12
    return {
      repoId: i + 1,
      fullName: `org-${(i % 25) + 1}/project-${i + 1}`,
      language,
      license,
      stars,
      forks,
      openIssues,
      archived,
    }
  })
  const w6Data = { repositories: repos }
  const rustMitOver50k = repos.filter(
    r => r.language === 'Rust' && r.license === 'MIT' && r.stars > 50000 && !r.archived,
  ).length
  const totalStarsAll = repos.reduce((s, r) => s + r.stars, 0)

  workloads.push({
    id: 'W6-github-repos-metrics',
    title: 'GitHub Repository Telemetry (200 repos)',
    category: 'High-Cardinality + Numeric Ranges',
    format: 'json',
    description: '200 open-source repositories with star/fork metrics, languages, and licenses.',
    data: w6Data,
    rawText: JSON.stringify(w6Data, null, 2),
    questions: [
      {
        id: 'w6-q1',
        workload: 'W6-github-repos-metrics',
        category: 'multi-clause-range',
        prompt:
          'How many unarchived Rust repositories with an MIT license have more than 50000 stars?',
        expectedSql: `SELECT COUNT(*) AS cnt FROM repositories WHERE language = 'Rust' AND license = 'MIT' AND archived = false AND stars > 50000`,
        groundTruth: String(rustMitOver50k),
        answerType: 'integer',
      },
      {
        id: 'w6-q2',
        workload: 'W6-github-repos-metrics',
        category: 'o1-header-agg',
        prompt: 'What is the sum of stars across all 200 repositories?',
        expectedSql: `SELECT SUM(stars) AS total_stars FROM repositories`,
        groundTruth: String(totalStarsAll),
        answerType: 'integer',
      },
      {
        id: 'w6-q3',
        workload: 'W6-github-repos-metrics',
        category: 'point-lookup',
        prompt: 'How many forks does repository org-10/project-35 have?',
        expectedSql: `SELECT forks FROM repositories WHERE fullName = 'org-10/project-35'`,
        groundTruth: String(repos[34]!.forks),
        answerType: 'integer',
      },
    ],
  })

  // ============================================================================
  // W7: Stripe Payment & Dispute Webhooks (200 events)
  // ============================================================================
  const rng7 = createRng(5007)
  const currencies = ['usd', 'eur', 'gbp']
  const paymentMethods = ['card', 'sepa_debit', 'ach_transfer', 'apple_pay']
  const charges = Array.from({ length: 200 }, (_, i) => {
    const currency = currencies[Math.floor(rng7() * currencies.length)]!
    const method = paymentMethods[Math.floor(rng7() * paymentMethods.length)]!
    const amountCents = 1500 + Math.floor(rng7() * 48500)
    const disputed = rng7() < 0.08
    const captured = !disputed || rng7() < 0.5
    return {
      chargeId: `ch_3N${String(10000 + i)}`,
      amountCents,
      currency,
      captured,
      disputed,
      paymentMethod: {
        type: method,
        country: ['US', 'DE', 'GB', 'FR'][i % 4]!,
      },
    }
  })
  const w7Data = { charges }
  const disputedUsdCardCnt = charges.filter(
    c => c.disputed && c.currency === 'usd' && c.paymentMethod.type === 'card',
  ).length
  const totalUsdAmount = charges
    .filter(c => c.currency === 'usd' && c.captured)
    .reduce((s, c) => s + c.amountCents, 0)

  workloads.push({
    id: 'W7-stripe-dispute-webhooks',
    title: 'Stripe Charge & Dispute Webhooks (200 events)',
    category: 'Financial Events & Nested Objects',
    format: 'json',
    description: '200 payment charge payloads with nested paymentMethod attributes and dispute flags.',
    data: w7Data,
    rawText: JSON.stringify(w7Data, null, 2),
    questions: [
      {
        id: 'w7-q1',
        workload: 'W7-stripe-dispute-webhooks',
        category: 'dot-path-nested',
        prompt: 'How many disputed usd charges used a card paymentMethod (paymentMethod.type = "card")?',
        expectedSql: `SELECT COUNT(*) AS cnt FROM charges WHERE disputed = true AND currency = 'usd' AND paymentMethod.type = 'card'`,
        groundTruth: String(disputedUsdCardCnt),
        answerType: 'integer',
      },
      {
        id: 'w7-q2',
        workload: 'W7-stripe-dispute-webhooks',
        category: 'multi-clause-range',
        prompt: 'What is the total amountCents of all captured usd charges?',
        expectedSql: `SELECT SUM(amountCents) AS total FROM charges WHERE currency = 'usd' AND captured = true`,
        groundTruth: String(totalUsdAmount),
        answerType: 'integer',
      },
    ],
  })

  // ============================================================================
  // W8: IAM Policies with Primitive Arrays (150 principals)
  // ============================================================================
  const rng8 = createRng(6008)
  const scopeSets = [
    ['read'],
    ['read', 'write'],
    ['read', 'write', 'deploy'],
    ['read', 'write', 'deploy', 'admin'],
    ['read', 'audit'],
  ]
  const principals = Array.from({ length: 150 }, (_, i) => {
    const scopes = scopeSets[Math.floor(rng8() * scopeSets.length)]!
    const mfaEnabled = rng8() > 0.25
    const group = ['platform', 'developers', 'contractors', 'security-ops'][i % 4]!
    return {
      principalId: `iam-user-${i + 1}`,
      group,
      mfaEnabled,
      scopes,
    }
  })
  const w8Data = { principals }
  const adminNoMfaCnt = principals.filter(
    p => p.scopes.includes('admin') && !p.mfaEnabled,
  ).length
  const atLeast3ScopesCnt = principals.filter(p => p.scopes.length >= 3).length

  workloads.push({
    id: 'W8-iam-primitive-arrays',
    title: 'IAM Principals & Scope Arrays (150 rows)',
    category: 'Primitive Arrays & Cardinality',
    format: 'json',
    description: '150 IAM principals with primitive string arrays (`scopes`) and MFA flags.',
    data: w8Data,
    rawText: JSON.stringify(w8Data, null, 2),
    questions: [
      {
        id: 'w8-q1',
        workload: 'W8-iam-primitive-arrays',
        category: 'primitive-array',
        prompt: 'How many principals have "admin" in their scopes but have mfaEnabled = false?',
        expectedSql: `SELECT COUNT(*) AS cnt FROM principals WHERE scopes LIKE '%admin%' AND mfaEnabled = false`,
        groundTruth: String(adminNoMfaCnt),
        answerType: 'integer',
      },
      {
        id: 'w8-q2',
        workload: 'W8-iam-primitive-arrays',
        category: 'primitive-array',
        prompt: 'How many principals have at least 3 scopes (scopes_count >= 3)?',
        expectedSql: `SELECT COUNT(*) AS cnt FROM principals WHERE scopes_count >= 3`,
        groundTruth: String(atLeast3ScopesCnt),
        answerType: 'integer',
      },
    ],
  })

  // ============================================================================
  // W9: Wide Enterprise CRM Export (150 rows x 30 columns)
  // ============================================================================
  const rng9 = createRng(7009)
  const accounts = Array.from({ length: 150 }, (_, i) => {
    const arrUsd = 12000 + Math.floor(rng9() * 250) * 1000
    const active = rng9() > 0.15
    const industry = ['Fintech', 'Healthcare', 'SaaS', 'Retail', 'Manufacturing'][i % 5]!
    const row: Record<string, unknown> = {
      accountId: `ACC-${2001 + i}`,
      industry,
      active,
      arrUsd,
      seatsLicensed: 10 + Math.floor(rng9() * 490),
      npsScore: 10 + Math.floor(rng9() * 90),
    }
    // Add 24 metric/feature columns (`metric_01` .. `metric_24`) for a 30-column wide table
    for (let m = 1; m <= 24; m++) {
      const colKey = `metric_${String(m).padStart(2, '0')}`
      row[colKey] = (i * m + 17) % 100
    }
    return row
  })
  const w9Data = { accounts }
  const activeFintechArr = accounts
    .filter(a => a.industry === 'Fintech' && a.active === true)
    .reduce((s, a) => s + Number(a.arrUsd), 0)

  workloads.push({
    id: 'W9-wide-enterprise-crm',
    title: 'Wide Enterprise CRM Export (150 rows x 30 cols)',
    category: 'Edge Case: Wide Table (30 Columns)',
    format: 'json',
    description: '150 enterprise accounts across 30 columns per record.',
    data: w9Data,
    rawText: JSON.stringify(w9Data, null, 2),
    questions: [
      {
        id: 'w9-q1',
        workload: 'W9-wide-enterprise-crm',
        category: 'multi-clause-range',
        prompt: 'What is the total arrUsd of active Fintech accounts?',
        expectedSql: `SELECT SUM(arrUsd) AS total_arr FROM accounts WHERE industry = 'Fintech' AND active = true`,
        groundTruth: String(activeFintechArr),
        answerType: 'integer',
      },
      {
        id: 'w9-q2',
        workload: 'W9-wide-enterprise-crm',
        category: 'point-lookup',
        prompt: 'What is the seatsLicensed value for account ACC-2025?',
        expectedSql: `SELECT seatsLicensed FROM accounts WHERE accountId = 'ACC-2025'`,
        groundTruth: String(accounts[24]!.seatsLicensed),
        answerType: 'integer',
      },
    ],
  })

  // ============================================================================
  // W10: Adversarial Unicode, Nulls & Whole-Float Financial Ledger (150 rows)
  // ============================================================================
  const rng10 = createRng(8010)
  const counterparties = [
    'Zürich FinTech AG',
    '東京スター銀行 (Tokyo Star)',
    'São Paulo Pagamentos 🚀',
    'München Rückversicherung',
    'Nordic Clearing Øresund',
  ]
  const settlements = Array.from({ length: 150 }, (_, i) => {
    const counterparty = counterparties[i % counterparties.length]!
    const settled = rng10() > 0.2
    // Mix whole numbers and fractional numbers
    const amount = i % 3 === 0 ? 5000 : Math.round((1000 + rng10() * 9000) * 100) / 100
    const memo = i % 4 === 0 ? null : `BATCH-REF-${i + 1}`
    return {
      txId: `TX-${9001 + i}`,
      counterparty,
      settled,
      amount,
      feeBps: i % 5 === 0 ? 0 : 15,
      memo,
    }
  })
  const w10Data = { settlements }
  const tokyoSettledCnt = settlements.filter(
    s => s.counterparty === '東京スター銀行 (Tokyo Star)' && s.settled,
  ).length
  const zeroFeeCnt = settlements.filter(s => s.feeBps === 0).length

  workloads.push({
    id: 'W10-unicode-financial-ledger',
    title: 'International Settlement Ledger (Unicode & Nulls, 150 rows)',
    category: 'Edge Case: Multi-Byte UTF-8, Nulls & Zeroes',
    format: 'json',
    description:
      '150 international settlements with Japanese/German/Portuguese/Emoji UTF-8 strings, explicit nulls, 0 values, and mixed whole/fractional floats.',
    data: w10Data,
    rawText: JSON.stringify(w10Data, null, 2),
    questions: [
      {
        id: 'w10-q1',
        workload: 'W10-unicode-financial-ledger',
        category: 'bitmap-filter',
        prompt: 'How many settled transactions are with counterparty "東京スター銀行 (Tokyo Star)"?',
        expectedSql: `SELECT COUNT(*) AS cnt FROM settlements WHERE counterparty = '東京スター銀行 (Tokyo Star)' AND settled = true`,
        groundTruth: String(tokyoSettledCnt),
        answerType: 'integer',
      },
      {
        id: 'w10-q2',
        workload: 'W10-unicode-financial-ledger',
        category: 'bitmap-filter',
        prompt: 'How many settlements have feeBps equal to 0?',
        expectedSql: `SELECT COUNT(*) AS cnt FROM settlements WHERE feeBps = 0`,
        groundTruth: String(zeroFeeCnt),
        answerType: 'integer',
      },
    ],
  })

  return workloads
}
