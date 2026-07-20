// Proxies GET /tiles/* to the TILES_BUCKET R2 binding (see wrangler.toml),
// with Range-request support so the browser's pmtiles client can do partial
// reads against a multi-GB archive instead of fetching the whole thing.
// Same-origin with the SPA (this Pages project), so no CORS headers needed.
export async function onRequestGet({ request, env, params }) {
  const key = params.path.join('/');

  // R2's `range` option needs an actual Range object or a Headers instance --
  // NOT the raw header string -- so the whole request.headers is passed
  // through and R2 parses the Range header out of it itself.
  const object = await env.TILES_BUCKET.get(key, { range: request.headers });
  if (!object) {
    return new Response('Not found', { status: 404 });
  }

  const headers = new Headers();
  object.writeHttpMetadata(headers);
  headers.set('etag', object.httpEtag);
  headers.set('accept-ranges', 'bytes');

  let status = 200;
  if (object.range) {
    status = 206;
    headers.set(
      'content-range',
      `bytes ${object.range.offset}-${object.range.offset + object.range.length - 1}/${object.size}`,
    );
  }

  return new Response(object.body, { status, headers });
}
