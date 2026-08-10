export const PROJECT_HOME_SANDBOX = [
  'allow-scripts',
  'allow-popups',
  'allow-popups-to-escape-sandbox',
].join(' ')

export const PROJECT_HOME_CSP = [
  "default-src 'none'",
  "style-src 'unsafe-inline'",
  "script-src 'unsafe-inline'",
  'img-src data: blob:',
  'media-src data: blob:',
  'font-src data:',
  'worker-src blob:',
  "connect-src 'none'",
  "frame-src 'none'",
  "object-src 'none'",
  "form-action 'none'",
  "base-uri 'none'",
].join('; ')

const PROJECT_HOME_CSP_META = `<meta http-equiv="Content-Security-Policy" content="${PROJECT_HOME_CSP.replaceAll('&', '&amp;').replaceAll('"', '&quot;')}">`

export function isolateProjectHomeDocument(source: string): string {
  const head = /<head\b[^>]*>/i.exec(source)
  if (head?.index !== undefined) {
    const offset = head.index + head[0].length
    return `${source.slice(0, offset)}${PROJECT_HOME_CSP_META}${source.slice(offset)}`
  }
  return source.replace(/<html\b[^>]*>/i, `$&<head>${PROJECT_HOME_CSP_META}</head>`)
}
