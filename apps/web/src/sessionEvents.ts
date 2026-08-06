import type { SessionEvent } from './types'

export function liveAssistantOutput(events: SessionEvent[], turnId: string): string {
  let output = ''
  for (const event of events) {
    if (event.turn_id !== turnId) continue
    if (event.type === 'assistant_message_reset') output = ''
    else if (event.type === 'assistant_message_delta') output += String(event.delta ?? '')
  }
  return output
}
