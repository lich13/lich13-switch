import assert from 'node:assert/strict';
import { randomUUID } from 'node:crypto';
import { spawn } from 'node:child_process';
import { createServer } from 'node:http';
import { once } from 'node:events';
import { existsSync } from 'node:fs';
import { mkdir, mkdtemp, readFile, realpath, rm, writeFile } from 'node:fs/promises';
import { homedir, tmpdir } from 'node:os';
import { join } from 'node:path';
import { after, before, test } from 'node:test';

// This is a local macOS integration check for the installed native Grok CLI.
// Every CLI child gets a fresh environment, an isolated home, and a Seatbelt
// profile whose only network exception is the mock server's loopback port.
const GROK_BIN = process.env.LICH13_TEST_GROK_BIN ?? join(homedir(), '.grok', 'bin', 'grok');
const SANDBOX_EXEC = '/usr/bin/sandbox-exec';
const FIXTURE_MODEL_ID = 'fixture-model-id';
const GROK_45_CONFIGURED_ID = 'grok-4.5';
const BUILTIN_MODEL_ID = 'grok-4.6';
const TEST_API_KEY = 'v025-native-fixture-only';
const REPLY = 'V025_NATIVE_MOCK_OK';
const MAIN_PROMPT = `Reply with exactly ${REPLY}. Do not use tools.`;
const CLI_TIMEOUT_MS = 20_000;

const models = [
  { key: 'fixture-api-a', modelId: FIXTURE_MODEL_ID, route: '/alias-a/v1', backend: 'chat_completions' },
  { key: 'fixture-api-b', modelId: FIXTURE_MODEL_ID, route: '/alias-b/v1', backend: 'chat_completions' },
  { key: 'fixture-grok-4-5', modelId: GROK_45_CONFIGURED_ID, route: '/alias-grok45/v1', backend: 'chat_completions' },
  { key: 'v025-native-responses', modelId: FIXTURE_MODEL_ID, route: '/protocol-responses/v1', backend: 'responses' },
  { key: 'v025-native-chat', modelId: FIXTURE_MODEL_ID, route: '/protocol-chat/v1', backend: 'chat_completions' },
  { key: 'v025-native-messages', modelId: FIXTURE_MODEL_ID, route: '/protocol-messages/v1', backend: 'messages' },
  { key: 'v025-native-grok46-api', modelId: BUILTIN_MODEL_ID, route: '/alias-grok46/v1', backend: 'chat_completions' },
];

let tempRoot;
let home;
let grokHome;
let leaderSocket;
let mockPort;
let server;
const requests = [];
const modelCatalogRequests = [];
const serverErrors = [];

function jsonResponse(res, body, status = 200) {
  res.writeHead(status, {
    'content-type': 'application/json; charset=utf-8',
    connection: 'close',
  });
  res.end(JSON.stringify(body));
}

function containsMainPrompt(value) {
  if (typeof value === 'string') return value.includes(MAIN_PROMPT);
  if (Array.isArray(value)) return value.some(containsMainPrompt);
  if (value && typeof value === 'object') return Object.values(value).some(containsMainPrompt);
  return false;
}

function userMessageHasMainPrompt(body) {
  const messages = Array.isArray(body.messages)
    ? body.messages
    : Array.isArray(body.input)
      ? body.input
      : [];
  return messages.some((message) => message?.role === 'user' && containsMainPrompt(message));
}

function sseResponse(res, events) {
  res.writeHead(200, {
    'content-type': 'text/event-stream; charset=utf-8',
    'cache-control': 'no-cache',
    connection: 'close',
  });
  for (const [event, data] of events) {
    if (event) res.write(`event: ${event}\n`);
    if (typeof data === 'string') {
      res.write(`data: ${data}\n\n`);
    } else {
      res.write(`data: ${JSON.stringify(data)}\n\n`);
    }
  }
  res.end();
}

function openAiChatEvents(model) {
  return [
    [null, {
      id: 'chatcmpl-v025-native',
      object: 'chat.completion.chunk',
      created: 1,
      model,
      choices: [{ index: 0, delta: { role: 'assistant', content: REPLY }, finish_reason: null }],
    }],
    [null, {
      id: 'chatcmpl-v025-native',
      object: 'chat.completion.chunk',
      created: 1,
      model,
      choices: [{ index: 0, delta: {}, finish_reason: 'stop' }],
      usage: { prompt_tokens: 1, completion_tokens: 1, total_tokens: 2 },
    }],
    [null, '[DONE]'],
  ];
}

function responsesEvents(model) {
  const responseId = 'resp_v025_native';
  const itemId = 'msg_v025_native';
  const output = [{
    id: itemId,
    type: 'message',
    status: 'completed',
    role: 'assistant',
    content: [{ type: 'output_text', annotations: [], text: REPLY }],
  }];
  const events = [];
  let sequenceNumber = 0;
  const addEvent = (event, fields) => events.push([event, {
    type: event,
    sequence_number: sequenceNumber++,
    ...fields,
  }]);
  addEvent('response.created', {
      type: 'response.created',
      response: { id: responseId, object: 'response', created_at: 1, status: 'in_progress', model, output: [] },
    });
  addEvent('response.output_item.added', {
      type: 'response.output_item.added',
      output_index: 0,
      item: { id: itemId, type: 'message', status: 'in_progress', role: 'assistant', content: [] },
    });
  addEvent('response.content_part.added', {
      type: 'response.content_part.added',
      item_id: itemId,
      output_index: 0,
      content_index: 0,
      part: { type: 'output_text', annotations: [], text: '' },
    });
  addEvent('response.output_text.delta', {
      type: 'response.output_text.delta',
      item_id: itemId,
      output_index: 0,
      content_index: 0,
      delta: REPLY,
    });
  addEvent('response.output_text.done', {
      type: 'response.output_text.done',
      item_id: itemId,
      output_index: 0,
      content_index: 0,
      text: REPLY,
    });
  addEvent('response.content_part.done', {
      type: 'response.content_part.done',
      item_id: itemId,
      output_index: 0,
      content_index: 0,
      part: { type: 'output_text', annotations: [], text: REPLY },
    });
  addEvent('response.output_item.done', {
      type: 'response.output_item.done',
      output_index: 0,
      item: output[0],
    });
  addEvent('response.completed', {
      type: 'response.completed',
      response: {
        id: responseId,
        object: 'response',
        created_at: 1,
        status: 'completed',
        model,
        output,
        usage: {
          input_tokens: 1,
          input_tokens_details: { cached_tokens: 0 },
          output_tokens: 1,
          output_tokens_details: { reasoning_tokens: 0 },
          total_tokens: 2,
        },
      },
    });
  return events;
}

function messagesEvents(model) {
  return [
    ['message_start', {
      type: 'message_start',
      message: {
        id: 'msg_v025_native',
        type: 'message',
        role: 'assistant',
        model,
        content: [],
        stop_reason: null,
        stop_sequence: null,
        usage: { input_tokens: 1, output_tokens: 0 },
      },
    }],
    ['content_block_start', {
      type: 'content_block_start',
      index: 0,
      content_block: { type: 'text', text: '' },
    }],
    ['content_block_delta', {
      type: 'content_block_delta',
      index: 0,
      delta: { type: 'text_delta', text: REPLY },
    }],
    ['content_block_stop', { type: 'content_block_stop', index: 0 }],
    ['message_delta', {
      type: 'message_delta',
      delta: { stop_reason: 'end_turn', stop_sequence: null },
      usage: { output_tokens: 1 },
    }],
    ['message_stop', { type: 'message_stop' }],
  ];
}

async function handleRequest(req, res) {
  try {
    const chunks = [];
    for await (const chunk of req) chunks.push(chunk);
    const rawBody = Buffer.concat(chunks).toString('utf8');
    let body = {};
    if (rawBody) {
      try {
        body = JSON.parse(rawBody);
      } catch {
        serverErrors.push(`non-JSON request for ${req.method} ${req.url}`);
        jsonResponse(res, { error: { message: 'fixture expects JSON' } }, 400);
        return;
      }
    }

    const url = new URL(req.url ?? '/', 'http://127.0.0.1');
    const isModelCatalog = req.method === 'GET' && url.pathname.endsWith('/models');
    const backend = isModelCatalog
      ? 'model_catalog'
      : url.pathname.endsWith('/chat/completions')
        ? 'chat_completions'
        : url.pathname.endsWith('/responses')
          ? 'responses'
          : url.pathname.endsWith('/messages')
            ? 'messages'
            : null;
    const request = {
      method: req.method,
      path: url.pathname,
      backend,
      model: typeof body.model === 'string' ? body.model : null,
      stream: body.stream === true,
      hasFixturePrompt: rawBody.includes(MAIN_PROMPT),
      fixturePromptInUserMessage: userMessageHasMainPrompt(body),
      messageRoles: (Array.isArray(body.messages) ? body.messages : Array.isArray(body.input) ? body.input : [])
        .map((message) => (typeof message?.role === 'string' ? message.role : null)),
      authorization: req.headers.authorization ?? null,
      apiKey: req.headers['x-api-key'] ?? null,
      anthropicVersion: req.headers['anthropic-version'] ?? null,
    };
    requests.push(request);

    if (isModelCatalog) {
      const ids = [...new Set(models.map((model) => model.modelId))];
      modelCatalogRequests.push({ path: url.pathname, ids });
      jsonResponse(res, {
        object: 'list',
        data: ids.map((id) => ({ id, object: 'model', created: 1, owned_by: 'local-fixture' })),
      });
      return;
    }

    if (req.method !== 'POST' || !backend) {
      jsonResponse(res, { error: { message: 'unexpected mock path' } }, 404);
      return;
    }

    if (backend === 'chat_completions') {
      if (body.stream === true) sseResponse(res, openAiChatEvents(body.model ?? FIXTURE_MODEL_ID));
      else jsonResponse(res, {
        id: 'chatcmpl-v025-native',
        object: 'chat.completion',
        created: 1,
        model: body.model ?? FIXTURE_MODEL_ID,
        choices: [{ index: 0, message: { role: 'assistant', content: REPLY }, finish_reason: 'stop' }],
        usage: { prompt_tokens: 1, completion_tokens: 1, total_tokens: 2 },
      });
      return;
    }

    if (backend === 'responses') {
      if (body.stream === true) sseResponse(res, responsesEvents(body.model ?? FIXTURE_MODEL_ID));
      else jsonResponse(res, {
        id: 'resp_v025_native',
        object: 'response',
        created_at: 1,
        status: 'completed',
        model: body.model ?? FIXTURE_MODEL_ID,
        output: [{
          id: 'msg_v025_native',
          type: 'message',
          status: 'completed',
          role: 'assistant',
          content: [{ type: 'output_text', annotations: [], text: REPLY }],
        }],
        output_text: REPLY,
        usage: {
          input_tokens: 1,
          input_tokens_details: { cached_tokens: 0 },
          output_tokens: 1,
          output_tokens_details: { reasoning_tokens: 0 },
          total_tokens: 2,
        },
      });
      return;
    }

    if (body.stream === true) sseResponse(res, messagesEvents(body.model ?? FIXTURE_MODEL_ID));
    else jsonResponse(res, {
      id: 'msg_v025_native',
      type: 'message',
      role: 'assistant',
      model: body.model ?? FIXTURE_MODEL_ID,
      content: [{ type: 'text', text: REPLY }],
      stop_reason: 'end_turn',
      stop_sequence: null,
      usage: { input_tokens: 1, output_tokens: 1 },
    });
  } catch {
    serverErrors.push(`mock handler failed for ${req.method} ${req.url}`);
    if (!res.headersSent) jsonResponse(res, { error: { message: 'mock handler failed' } }, 500);
    else res.destroy();
  }
}

function freshChildEnv() {
  const env = {
    HOME: home,
    GROK_HOME: grokHome,
    PATH: '/usr/bin:/bin:/usr/sbin:/sbin',
    TMPDIR: tempRoot,
    TERM: 'dumb',
    NO_COLOR: '1',
    GROK_AGENT_DASHBOARD: '0',
    GROK_DISABLE_AUTOUPDATER: '1',
  };
  const credentialish = Object.keys(env).filter((key) => /API_KEY|TOKEN|SECRET|AUTH|COOKIE|PROXY/i.test(key));
  assert.deepEqual(credentialish, [], 'the native CLI environment must not inherit credential or proxy variables');
  return env;
}

function safeOutput(value) {
  let output = String(value ?? '');
  for (const sensitive of [TEST_API_KEY, tempRoot, homedir(), GROK_BIN, process.env.LICH13_TEST_GROK_BIN]) {
    if (sensitive) output = output.replaceAll(sensitive, '<redacted>');
  }
  return output;
}

function sandboxProfile() {
  // `deny default` blocks all external addresses; the only network exception
  // is this run's ephemeral local mock port. There is no `allow default` rule.
  return [
    '(version 1)',
    '(deny default)',
    '(allow file-read*)',
    `(allow file-write* (subpath "${tempRoot}") (literal "/dev/null"))`,
    '(allow process-exec)',
    '(allow process-fork)',
    '(allow sysctl-read)',
    `(allow network-outbound (remote ip "localhost:${mockPort}"))`,
  ].join(' ');
}

function signalGroup(pid, signal) {
  if (!pid) return;
  try {
    process.kill(-pid, signal);
  } catch (error) {
    if (error?.code !== 'ESRCH') throw error;
  }
}

async function stopProcessGroup(pid) {
  if (!pid) return;
  signalGroup(pid, 'SIGTERM');
  const deadline = Date.now() + 700;
  while (Date.now() < deadline) {
    try {
      process.kill(-pid, 0);
      await new Promise((resolve) => setTimeout(resolve, 50));
    } catch (error) {
      if (error?.code === 'ESRCH') return;
      throw error;
    }
  }
  signalGroup(pid, 'SIGKILL');
}

async function runGrok(args, timeoutMs = CLI_TIMEOUT_MS) {
  const grokArgs = [
    '--leader-socket', leaderSocket,
    '--cwd', tempRoot,
    ...args,
  ];
  const child = spawn(SANDBOX_EXEC, ['-p', sandboxProfile(), GROK_BIN, ...grokArgs], {
    cwd: tempRoot,
    env: freshChildEnv(),
    detached: true,
    stdio: ['ignore', 'pipe', 'pipe'],
  });
  let stdout = '';
  let stderr = '';
  let timedOut = false;
  let timeoutHandle;
  let escalationHandle;
  const maxOutputChars = 1_000_000;
  child.stdout.setEncoding('utf8');
  child.stderr.setEncoding('utf8');
  child.stdout.on('data', (chunk) => {
    if (stdout.length < maxOutputChars) stdout += chunk.slice(0, maxOutputChars - stdout.length);
  });
  child.stderr.on('data', (chunk) => {
    if (stderr.length < maxOutputChars) stderr += chunk.slice(0, maxOutputChars - stderr.length);
  });

  let result;
  try {
    result = await new Promise((resolve, reject) => {
      child.once('error', reject);
      child.once('close', (code, signal) => resolve({ code, signal }));
      timeoutHandle = setTimeout(() => {
        timedOut = true;
        signalGroup(child.pid, 'SIGTERM');
        escalationHandle = setTimeout(() => signalGroup(child.pid, 'SIGKILL'), 700);
        escalationHandle.unref();
      }, timeoutMs);
    });
  } finally {
    clearTimeout(timeoutHandle);
    clearTimeout(escalationHandle);
    await stopProcessGroup(child.pid);
  }

  return { ...result, stdout, stderr, timedOut };
}

function configToml(baseUrl) {
  const allowlist = [...models.map((model) => `"${model.key}"`), `"${BUILTIN_MODEL_ID}"`].join(', ');
  const modelSections = models.map((model) => {
    const responseOnly = model.backend === 'responses' ? 'reasoning_summary = "none"\n' : '';
    const messageHeaders = model.backend === 'messages'
      ? `extra_headers = { "x-api-key" = "${TEST_API_KEY}", "anthropic-version" = "2023-06-01" }\n`
      : '';
    return [
      `[model.${model.key}]`,
      `model = "${model.modelId}"`,
      `name = "${model.key}"`,
      `base_url = "${baseUrl}${model.route}"`,
      `api_key = "${TEST_API_KEY}"`,
      `api_backend = "${model.backend}"`,
      'context_window = 8192',
      'max_retries = 0',
      'hidden = false',
      responseOnly.trimEnd(),
      messageHeaders.trimEnd(),
    ].filter(Boolean).join('\n');
  });

  return [
    '[cli]',
    'auto_update = false',
    '',
    '[features]',
    'telemetry = false',
    'feedback = false',
    'managed_config = false',
    'remote_fetch = false',
    'codebase_indexing = false',
    'session_recap = false',
    'title_refresh = false',
    'turn_summary = false',
    '',
    '[session]',
    'load_envrc = false',
    '',
    '[models]',
    `default = "${models[0].key}"`,
    `allowed_models = [${allowlist}]`,
    '',
    ...modelSections,
    '',
  ].join('\n');
}

function invokeArgs(alias) {
  const args = [
    '--no-subagents',
    '--disable-web-search',
    '--max-turns', '1',
    '--output-format', 'plain',
    '--session-id', randomUUID(),
  ];
  if (alias) args.push('--model', alias);
  args.push('--single', MAIN_PROMPT);
  return args;
}

function assertCliSucceeded(result, context) {
  assert.equal(result.timedOut, false, `${context}: native CLI timed out`);
  assert.equal(result.code, 0, `${context}: ${safeOutput(result.stderr || result.stdout)}`);
  assert.match(`${safeOutput(result.stdout)}\n${safeOutput(result.stderr)}`, new RegExp(REPLY));
}

function modelEndpointPath(model) {
  const suffix = model.backend === 'chat_completions'
    ? '/chat/completions'
    : model.backend === 'responses'
      ? '/responses'
      : '/messages';
  return `${model.route}${suffix}`;
}

function isConfiguredModelRequest(model, entry, expectedModelId = model.modelId) {
  return entry.method === 'POST'
    && entry.path === modelEndpointPath(model)
    && entry.backend === model.backend
    && entry.hasFixturePrompt
    && entry.fixturePromptInUserMessage
    && entry.model === expectedModelId;
}

async function runModelCli(model, args) {
  const requestStart = requests.length;
  const result = await runGrok(args);
  const observedRequests = requests.slice(requestStart);
  const configuredIdRequests = observedRequests.filter((entry) => isConfiguredModelRequest(model, entry));
  const configuredRequestSet = new Set(configuredIdRequests);
  const summarizeRequest = (entry) => ({
    method: entry.method,
    path: entry.path,
    protocol: entry.backend,
    model: entry.model,
    stream: entry.stream,
    fixturePromptMatched: entry.hasFixturePrompt,
    fixturePromptInUserMessage: entry.fixturePromptInUserMessage,
    messageRoles: entry.messageRoles,
    authorizationHeaderPresent: entry.authorization !== null,
    xApiKeyHeaderPresent: entry.apiKey !== null,
  });
  const report = {
    entryKey: model.key,
    requestStart,
    requestEnd: requests.length,
    requestCount: observedRequests.length,
    configuredIdRequestCount: configuredIdRequests.length,
    configuredIdRequests: configuredIdRequests.map(summarizeRequest),
    otherRequests: observedRequests
      .filter((entry) => !configuredRequestSet.has(entry))
      .map(summarizeRequest),
  };
  console.log(`[native-mock-observation] ${JSON.stringify(report)}`);
  return { result, requestStart, requests: observedRequests, configuredIdRequests };
}

function assertModelRoute(model, run, expectedModelId = model.modelId) {
  const matchedRequests = run.requests.filter((entry) => isConfiguredModelRequest(model, entry, expectedModelId));
  assert.ok(
    matchedRequests.length > 0,
    `${model.key}: no request with the fixture main prompt and configured model ID reached the selected local endpoint`,
  );
  for (const request of matchedRequests) {
    assert.equal(request.method, 'POST', `${model.key}: expected an inference POST`);
    assert.equal(request.backend, model.backend, `${model.key}: API protocol did not match the selected model`);
    assert.ok(
      request.authorization === `Bearer ${TEST_API_KEY}`
        || request.apiKey === TEST_API_KEY,
      `${model.key}: request carried unexpected authentication material`,
    );
    assert.ok(
      request.authorization !== null || request.apiKey !== null,
      `${model.key}: no authentication header field was present`,
    );
    assert.ok(
      typeof request.model === 'string' && request.model.length > 0,
      `${model.key}: no wire model ID was present on the fixture main prompt request`,
    );
    if (model.backend === 'messages') {
      assert.equal(request.apiKey, TEST_API_KEY, `${model.key}: messages API key header did not use the fixture key`);
      assert.equal(request.anthropicVersion, '2023-06-01', `${model.key}: messages API version header is missing`);
    }
  }
  return matchedRequests;
}

function assertModelRequest(model, run, expectedModelId = model.modelId) {
  const matchedRequests = assertModelRoute(model, run, expectedModelId);
  for (const request of matchedRequests) {
    assert.equal(request.model, expectedModelId, `${model.key}: unexpected model ID in the API request`);
  }
  return matchedRequests;
}

let detectedGrokVersion = '';

function assertMinSemver(version, minimum = [1, 0, 52]) {
  const parts = version.split('.').map((part) => Number.parseInt(part, 10));
  assert.equal(parts.length, 3, `unexpected grok semver: ${version}`);
  assert.ok(parts.every(Number.isInteger), `non-numeric grok semver: ${version}`);
  for (let i = 0; i < 3; i += 1) {
    if (parts[i] > minimum[i]) return;
    if (parts[i] < minimum[i]) {
      assert.fail(`native grok version ${version} is older than ${minimum.join('.')}`);
    }
  }
}

before(async () => {
  assert.equal(process.platform, 'darwin', 'this native CLI check is only prepared for macOS');
  assert.ok(existsSync(GROK_BIN), 'native Grok binary not found at the configured test path');
  assert.ok(existsSync(SANDBOX_EXEC), 'macOS sandbox-exec is required to enforce loopback-only networking');
  assert.equal(existsSync('/etc/grok/managed_config.toml'), false, 'system managed config is present; inspect it before running');
  assert.equal(existsSync('/etc/grok/requirements.toml'), false, 'system requirements are present; inspect them before running');

  tempRoot = await mkdtemp(join(tmpdir(), 'lich13-v025-grok-native-'));
  tempRoot = await realpath(tempRoot);
  home = join(tempRoot, 'home');
  grokHome = join(tempRoot, 'grok-home');
  leaderSocket = join(grokHome, 'native-test-leader.sock');
  await mkdir(home, { recursive: true });
  await mkdir(grokHome, { recursive: true });

  server = createServer((req, res) => {
    void handleRequest(req, res);
  });
  server.listen(0, '127.0.0.1');
  await once(server, 'listening');
  mockPort = server.address().port;
  const baseUrl = `http://localhost:${mockPort}`;
  await writeFile(join(grokHome, 'config.toml'), configToml(baseUrl), { mode: 0o600, flag: 'wx' });

  const version = await runGrok(['version']);
  assert.equal(version.timedOut, false, 'native Grok version check timed out');
  assert.equal(version.code, 0, safeOutput(version.stderr || version.stdout));
  const versionOutput = `${version.stdout}\n${version.stderr}`;
  const match = versionOutput.match(/grok (\d+\.\d+\.\d+)\s+\([0-9a-f]+\)/i);
  assert.ok(match, `unexpected grok version output: ${safeOutput(versionOutput)}`);
  detectedGrokVersion = match[1];
  assertMinSemver(detectedGrokVersion);
});

after(async () => {
  if (server?.listening) {
    server.closeAllConnections?.();
    await new Promise((resolve) => server.close(resolve));
  }
  if (tempRoot) {
    await rm(tempRoot, { recursive: true });
  }
});

test('isolated config exposes the explicit API model aliases to the native picker', async () => {
  const result = await runGrok(['models']);
  assert.equal(result.timedOut, false, 'grok models timed out');
  assert.equal(result.code, 0, `${result.stderr || result.stdout}`);
  const listing = `${result.stdout}\n${result.stderr}`;
  for (const model of models) {
    assert.ok(listing.includes(model.key), `grok models did not list ${model.key}`);
  }
  assert.ok(listing.includes(BUILTIN_MODEL_ID), 'grok models did not list the built-in model alongside API aliases');
});

test('native inspector reports only the isolated config layer', async () => {
  const result = await runGrok(['inspect', '--json']);
  assert.equal(result.timedOut, false, 'native inspector timed out');
  assert.equal(result.code, 0, safeOutput(result.stderr || result.stdout));
  let inspection;
  try {
    inspection = JSON.parse(result.stdout);
  } catch {
    assert.fail('grok inspect --json did not return JSON');
  }
  assert.equal(inspection.grokVersion, detectedGrokVersion);
  const layers = inspection.configSources?.layers ?? [];
  assert.equal(layers.length, 1, 'inspector should report only the isolated user config layer');
  assert.equal(layers[0].role, 'user', 'inspector should report the isolated user config layer');
  assert.ok(layers[0].path === join(grokHome, 'config.toml'), 'inspector did not load the generated temp config');
  const config = await readFile(join(grokHome, 'config.toml'), 'utf8');
  assert.equal(/^\s*\[endpoints\]\s*$/m.test(config), false, 'isolated config must not define global endpoints');
  assert.equal(/^\s*preferred_method\s*=/m.test(config), false, 'isolated config must not define preferred_method');
  for (const model of models.slice(0, 2)) {
    assert.ok(
      config.includes(`[model.${model.key}]\nmodel = "${FIXTURE_MODEL_ID}"`),
      `${model.key}: generated config did not contain the requested fixture model ID`,
    );
  }
});

test('the configured default alias routes to its endpoint and sends the configured fixture model ID', async () => {
  const model = models[0];
  const run = await runModelCli(model, invokeArgs());
  assertCliSucceeded(run.result, 'default model');
  assertModelRequest(model, run);
  assert.ok(run.requests.length > 0, 'default alias: mock server received no request');
});

for (const model of models.slice(0, 2)) {
  test(`${model.key} is selectable and sends the shared unknown fixture model ID`, async () => {
    const run = await runModelCli(model, invokeArgs(model.key));
    assertCliSucceeded(run.result, model.key);
    assertModelRequest(model, run);
    assert.ok(run.requests.length > 0, `${model.key}: mock server received no request`);
  });
}

test('a grok-4.5 custom entry dispatches the configured grok-4.5 model ID', async () => {
  const model = models[2];
  const run = await runModelCli(model, invokeArgs(model.key));
  assertCliSucceeded(run.result, model.key);
  assertModelRequest(model, run);
});

test('a custom API alias can share the built-in grok-4.6 ID and route to its own endpoint', async () => {
  const model = models.at(-1);
  assert.equal(model.modelId, BUILTIN_MODEL_ID);
  const run = await runModelCli(model, invokeArgs(model.key));
  assertCliSucceeded(run.result, model.key);
  assertModelRequest(model, run, BUILTIN_MODEL_ID);
});

for (const model of models.slice(3, -1)) {
  test(`the native CLI completes the mocked ${model.backend} protocol`, async () => {
    const run = await runModelCli(model, invokeArgs(model.key));
    assertCliSucceeded(run.result, model.key);
    assertModelRequest(model, run);
  });
}

test('mock requests contain only the fixture credential and no server-side errors occurred', () => {
  assert.deepEqual(serverErrors, []);
  assert.ok(requests.length >= models.length + 1, 'mock server did not receive each configured model request');
  assert.ok(
    modelCatalogRequests.every((request) => !request.ids.includes(BUILTIN_MODEL_ID)),
    'the local models fixture must not advertise the built-in model',
  );
  assert.equal(
    modelCatalogRequests.length,
    0,
    'picker visibility did not invoke the model catalog or initiate an official inference request',
  );
  for (const request of requests) {
    for (const value of [request.authorization, request.apiKey]) {
      if (value == null) continue;
      assert.ok(
        value === `Bearer ${TEST_API_KEY}` || value === TEST_API_KEY,
        'mock server received authentication material outside the test fixture',
      );
    }
  }
  assert.equal(existsSync(join(grokHome, 'auth.json')), false, 'isolated run unexpectedly created an OAuth credential file');
});
