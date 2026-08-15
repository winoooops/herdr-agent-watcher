import { existsSync, mkdtempSync, readFileSync, rmSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { afterEach, describe, expect, test, vi } from 'vitest'

interface BridgeHooks {
  event: (params: {
    event: {
      type: string
      properties: Record<string, unknown>
    }
  }) => Promise<void>
  'tool.execute.before': (
    input: { sessionID: string; tool: string; callID: string },
    output: { args: Record<string, unknown> }
  ) => Promise<void>
}

const importBridge = async (): Promise<BridgeHooks> => {
  const module = await import('./agent-watcher-opencode-bridge.ts')

  return module.AgentWatcherOpencodeBridge({}) as Promise<BridgeHooks>
}

describe('AgentWatcherOpencodeBridge', () => {
  afterEach(() => {
    vi.resetModules()
    delete process.env.AGENT_WATCHER_OPENCODE_BRIDGE_DIR
  })

  test('redacts credential key args before writing bridge JSONL', async () => {
    const dir = mkdtempSync(join(tmpdir(), 'agent-watcher-opencode-bridge-'))
    process.env.AGENT_WATCHER_OPENCODE_BRIDGE_DIR = dir

    try {
      const bridge = await importBridge()

      await bridge['tool.execute.before'](
        { sessionID: 'session-redact', tool: 'cloud', callID: 'call-redact' },
        {
          args: {
            accessKey: 'access-key-value',
            apiSecretKey: 'api-secret-key-value',
            awsSecretAccessKey: 'aws-secret-access-key-value',
            authKey: 'auth-key-value',
            encryptionKey: 'encryption-key-value',
            kmsEncryptionKey: 'kms-encryption-key-value',
            myAccessKey: 'my-access-key-value',
            safeKeyLabel: 'customer-key-alias',
            secretAccessKey: 'secret-access-key-value',
            secretKey: 'secret-key-value',
            signingKey: 'signing-key-value',
            webhookSigningKey: 'webhook-signing-key-value',
          },
        }
      )

      const line = readFileSync(join(dir, 'session-redact.jsonl'), 'utf8')
      const record = JSON.parse(line) as { args: Record<string, unknown> }

      expect(record.args).toMatchObject({
        accessKey: '[redacted]',
        apiSecretKey: '[redacted]',
        awsSecretAccessKey: '[redacted]',
        authKey: '[redacted]',
        encryptionKey: '[redacted]',
        kmsEncryptionKey: '[redacted]',
        myAccessKey: '[redacted]',
        safeKeyLabel: 'customer-key-alias',
        secretAccessKey: '[redacted]',
        secretKey: '[redacted]',
        signingKey: '[redacted]',
        webhookSigningKey: '[redacted]',
      })
    } finally {
      rmSync(dir, { recursive: true, force: true })
    }
  })

  test('drops unsafe session ids before writing session or index files', async () => {
    const dir = mkdtempSync(join(tmpdir(), 'agent-watcher-opencode-bridge-'))
    process.env.AGENT_WATCHER_OPENCODE_BRIDGE_DIR = dir

    try {
      const bridge = await importBridge()

      await bridge.event({
        event: {
          type: 'session.created',
          properties: {
            info: {
              id: '../escaped',
              directory: '/workspace/project',
            },
          },
        },
      })

      expect(existsSync(join(dir, 'index.jsonl'))).toBe(false)
      expect(existsSync(join(dir, '..', 'escaped.jsonl'))).toBe(false)
    } finally {
      rmSync(dir, { recursive: true, force: true })
    }
  })

  test('writes only notification routing fields for permission events', async () => {
    const dir = mkdtempSync(join(tmpdir(), 'agent-watcher-opencode-bridge-'))
    process.env.AGENT_WATCHER_OPENCODE_BRIDGE_DIR = dir

    try {
      const bridge = await importBridge()

      await bridge.event({
        event: {
          type: 'permission.asked',
          properties: {
            id: 'permission-1',
            sessionID: 'session-permission',
            permission: 'bash',
            metadata: { secret: 'must-not-cross' },
          },
        },
      })

      const line = readFileSync(join(dir, 'session-permission.jsonl'), 'utf8')
      const record = JSON.parse(line) as { data: Record<string, unknown> }

      expect(record.data).toEqual({
        id: 'permission-1',
        sessionID: 'session-permission',
      })
    } finally {
      rmSync(dir, { recursive: true, force: true })
    }
  })

  test('writes coerced directory values to index rows', async () => {
    const dir = mkdtempSync(join(tmpdir(), 'agent-watcher-opencode-bridge-'))
    process.env.AGENT_WATCHER_OPENCODE_BRIDGE_DIR = dir

    try {
      const bridge = await importBridge()

      await bridge.event({
        event: {
          type: 'session.created',
          properties: {
            info: {
              id: 'session-index',
              slug: 'Index Session',
            },
          },
        },
      })

      const line = readFileSync(join(dir, 'index.jsonl'), 'utf8')
      const record = JSON.parse(line) as { directory: string }

      expect(record.directory).toBe('')
    } finally {
      rmSync(dir, { recursive: true, force: true })
    }
  })

  test('tail-clamps assistant text before writing assistant text rows', async () => {
    const dir = mkdtempSync(join(tmpdir(), 'agent-watcher-opencode-bridge-'))
    process.env.AGENT_WATCHER_OPENCODE_BRIDGE_DIR = dir

    try {
      const bridge = await importBridge()
      const prefix = 'x'.repeat(40_000)
      const tail = 'assistant tail reply'

      await bridge.event({
        event: {
          type: 'message.part.updated',
          properties: {
            part: {
              type: 'text',
              sessionID: 'session-assistant-text',
              messageID: 'message-assistant',
              id: 'part-large',
              text: `${prefix}${tail}`,
            },
          },
        },
      })
      await bridge.event({
        event: {
          type: 'message.updated',
          properties: {
            info: {
              role: 'assistant',
              sessionID: 'session-assistant-text',
              id: 'message-assistant',
            },
          },
        },
      })
      await bridge.event({
        event: {
          type: 'session.idle',
          properties: {
            sessionID: 'session-assistant-text',
          },
        },
      })

      const records = readFileSync(
        join(dir, 'session-assistant-text.jsonl'),
        'utf8'
      )
        .trim()
        .split('\n')
        .map(
          (line) =>
            JSON.parse(line) as {
              type: string
              data: { text: string }
            }
        )
      const record = records.find(
        (candidate) => candidate.type === 'assistant.text'
      )

      expect(record).toBeDefined()
      expect(record?.data.text.length).toBeLessThanOrEqual(32768)
      expect(record?.data.text.endsWith(tail)).toBe(true)
    } finally {
      rmSync(dir, { recursive: true, force: true })
    }
  })
})
