import { describe, expect, it } from 'vitest'
import {
  isolateProjectHomeDocument,
  PROJECT_HOME_CSP,
  PROJECT_HOME_SANDBOX,
} from './projectHome'

describe('Project Home document isolation', () => {
  it('allows presentation code without sharing the application origin', () => {
    expect(PROJECT_HOME_SANDBOX).toContain('allow-scripts')
    expect(PROJECT_HOME_SANDBOX).not.toContain('allow-same-origin')
    expect(PROJECT_HOME_SANDBOX).not.toContain('allow-top-navigation')
    expect(PROJECT_HOME_CSP).toContain("style-src 'unsafe-inline'")
    expect(PROJECT_HOME_CSP).toContain("script-src 'unsafe-inline'")
    expect(PROJECT_HOME_CSP).toContain('img-src data: blob:')
  })

  it('does not give generated documents a background network channel', () => {
    expect(PROJECT_HOME_CSP).toContain("default-src 'none'")
    expect(PROJECT_HOME_CSP).toContain("connect-src 'none'")
    expect(PROJECT_HOME_CSP).toContain("form-action 'none'")
    expect(PROJECT_HOME_CSP).toContain("base-uri 'none'")

    const source = '<!DOCTYPE html><html><head><style>body{color:red}</style></head><body><script>draw()</script></body></html>'
    const isolated = isolateProjectHomeDocument(source)
    expect(isolated).toContain('<head><meta http-equiv="Content-Security-Policy"')
    expect(isolated).toContain('<style>body{color:red}</style>')
    expect(isolated).toContain('<script>draw()</script>')
  })

  it('creates a head only when the generated document omitted one', () => {
    expect(isolateProjectHomeDocument('<html><body>Result</body></html>')).toContain(
      '<html><head><meta http-equiv="Content-Security-Policy"',
    )
  })
})
