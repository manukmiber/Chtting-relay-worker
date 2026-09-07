import http from 'node:http';

/**
 * A stand-in OpenAI-compatible backend. It echoes back what it was asked so
 * tests can assert on name translation, prompt injection and reshaping, and it
 * can stream with a controllable delay so TTFT and tokens/sec are measurable.
 */
export async function startMockBackend({
  text = 'Hello from the backend.',
  reasoning = '',
  chunkDelayMs = 5,
  firstChunkDelayMs = 20,
  usage = { prompt_tokens: 11, completion_tokens: 7, total_tokens: 18 },
  status = 200,
  errorBody = null,
} = {}) {
  const received = [];

  const server = http.createServer(async (req, res) => {
    const chunks = [];
    for await (const c of req) chunks.push(c);
    const body = chunks.length ? JSON.parse(Buffer.concat(chunks).toString('utf8')) : {};
    received.push({ url: req.url, headers: req.headers, body });

    if (req.url.endsWith('/models')) {
      res.writeHead(200, { 'content-type': 'application/json' });
      return res.end(JSON.stringify({ data: [{ id: 'Deepseek-v4-flash-0731' }] }));
    }

    if (status !== 200) {
      res.writeHead(status, { 'content-type': 'application/json' });
      return res.end(JSON.stringify(errorBody ?? { error: { message: 'mock failure' } }));
    }

    if (!body.stream) {
      res.writeHead(200, { 'content-type': 'application/json' });
      return res.end(JSON.stringify({
        id: 'chatcmpl-mock',
        object: 'chat.completion',
        created: 1700000000,
        model: body.model,
        system_fingerprint: 'fp_mock',
        choices: [{
          index: 0,
          message: { role: 'assistant', content: text, ...(reasoning ? { reasoning_content: reasoning } : {}) },
          finish_reason: 'stop',
        }],
        ...(usage ? { usage } : {}),
      }));
    }

    res.writeHead(200, {
      'content-type': 'text/event-stream',
      'cache-control': 'no-cache',
      connection: 'keep-alive',
    });
    const send = (obj) => res.write(`data: ${JSON.stringify(obj)}\n\n`);
    const base = { id: 'chatcmpl-mock', object: 'chat.completion.chunk', created: 1700000000, model: body.model };

    await sleep(firstChunkDelayMs);
    send({ ...base, choices: [{ index: 0, delta: { role: 'assistant', content: '' }, finish_reason: null }] });

    if (reasoning) {
      for (const piece of chunkify(reasoning, 6)) {
        await sleep(chunkDelayMs);
        send({ ...base, choices: [{ index: 0, delta: { reasoning_content: piece }, finish_reason: null }] });
      }
    }
    for (const piece of chunkify(text, 6)) {
      await sleep(chunkDelayMs);
      send({ ...base, choices: [{ index: 0, delta: { content: piece }, finish_reason: null }] });
    }
    send({ ...base, choices: [{ index: 0, delta: {}, finish_reason: 'stop' }] });
    if (usage && body.stream_options?.include_usage) send({ ...base, choices: [], usage });
    res.write('data: [DONE]\n\n');
    return res.end();
  });

  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  const { port } = server.address();
  return {
    server,
    port,
    baseUrl: `http://127.0.0.1:${port}/v1`,
    received,
    close: () => new Promise((r) => server.close(r)),
  };
}

function chunkify(s, size) {
  const out = [];
  for (let i = 0; i < s.length; i += size) out.push(s.slice(i, i + size));
  return out;
}

function sleep(ms) {
  return new Promise((r) => setTimeout(r, ms));
}
