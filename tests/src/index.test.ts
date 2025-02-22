import { describe, expect, test, beforeAll, afterAll } from 'vitest'
import { DocumentManager } from '@y-sweet/sdk'
import { createYjsProvider as createYjsProvider_, YSweetProviderParams } from '@y-sweet/react'
import { WebSocket } from 'ws'
import * as Y from 'yjs'
import { Server, ServerConfiguration } from './server'

/**
 * Wraps `createYjsProvider` with a polyfill for `WebSocket` and
 * disables the broadcast channel, which gets in the way of tests
 * because it bypasses the network for local changes.
 */
function createYjsProvider(
  doc: Y.Doc,
  clientToken: { url: string; doc: string; token?: string },
  extraOptions: YSweetProviderParams,
) {
  extraOptions = {
    WebSocketPolyfill: require('ws'),
    // Broadcast channel prevents us from mocking separate clients.
    disableBc: true,
    ...extraOptions,
  }
  return createYjsProvider_(doc, clientToken, extraOptions)
}

const CONFIGURATIONS: ServerConfiguration[] = [
  { useAuth: false, server: 'native' },
  { useAuth: true, server: 'native' },
  { useAuth: false, server: 'worker' },
  { useAuth: true, server: 'worker' },
]

let S3_ACCESS_KEY_ID = process.env.Y_SWEET_S3_ACCESS_KEY_ID
let S3_SECRET_KEY = process.env.Y_SWEET_S3_SECRET_KEY
let S3_REGION = process.env.Y_SWEET_S3_REGION
let S3_BUCKET_PREFIX = process.env.Y_SWEET_S3_BUCKET_PREFIX
let S3_BUCKET_NAME = process.env.Y_SWEET_S3_BUCKET_NAME
//run s3 tests if env vars set
if (S3_ACCESS_KEY_ID && S3_REGION && S3_SECRET_KEY && S3_BUCKET_PREFIX && S3_BUCKET_NAME) {
  CONFIGURATIONS.push({
    useAuth: true,
    server: 'worker',
    s3: {
      bucket_name: S3_BUCKET_NAME,
      bucket_prefix: S3_BUCKET_PREFIX,
      aws_access_key_id: S3_ACCESS_KEY_ID,
      aws_region: S3_REGION,
      aws_secret_key: S3_SECRET_KEY,
    },
  })
}

let MINIO_PORT = process.env.Y_SWEET_MINIO_PORT
//run s3 tests using minio if available
if (MINIO_PORT && S3_BUCKET_NAME && S3_BUCKET_PREFIX) {
  CONFIGURATIONS.push({
    useAuth: true,
    server: 'worker',
    s3: {
      bucket_name: S3_BUCKET_NAME,
      bucket_prefix: S3_BUCKET_PREFIX,
      endpoint: `http://localhost:${MINIO_PORT}`,
      aws_access_key_id: 'minioadmin',
      aws_region: 'minio',
      aws_secret_key: 'minioadmin',
    },
  })
}

const TEN_MINUTES_IN_MS = 10 * 60 * 1_000

describe.each(CONFIGURATIONS)(
  'Test $server (auth: $useAuth, s3: $s3)',
  (configuration: ServerConfiguration) => {
    let SERVER: Server
    let DOCUMENT_MANANGER: DocumentManager

    beforeAll(async () => {
      SERVER = new Server(configuration)
      DOCUMENT_MANANGER = new DocumentManager(SERVER.connectionString())

      await SERVER.waitForReady()
    }, TEN_MINUTES_IN_MS)

    afterAll(() => {
      SERVER.cleanup()
    })

    test('Check store status', async () => {
      const result = await DOCUMENT_MANANGER.checkStore()
      expect(result).toEqual({ ok: true })
    })

    test('Create new doc', async () => {
      const result = await DOCUMENT_MANANGER.createDoc()
      expect(typeof result.doc).toBe('string')
    })

    test('Attempt to access non-existing doc', async () => {
      await expect(DOCUMENT_MANANGER.getClientToken('foobar', {})).rejects.toThrow('404')

      // When running Cloudflare's workerd locally, sometimes the call following
      // the 404 will fail with a 500.
      // Not sure why, but this is a workaround.
      await DOCUMENT_MANANGER.createDoc().catch(() => {})
    })

    test('Create and connect to doc', async () => {
      const docResult = await DOCUMENT_MANANGER.createDoc()
      const key = await DOCUMENT_MANANGER.getClientToken(docResult, {})

      if (configuration.useAuth) {
        expect(key.token).toBeDefined()
      } else {
        expect(key.token).toBeUndefined()
      }

      const doc = new Y.Doc()
      const provider = createYjsProvider(doc, key, {})

      await new Promise((resolve, reject) => {
        provider.on('synced', resolve)
        provider.on('syncing', reject)
      })
    })

    test('Create a doc by specifying a name', async () => {
      const docResult = await DOCUMENT_MANANGER.createDoc({
        doc: 'mydoc123',
      })

      expect(docResult.doc).toBe('mydoc123')
    })

    test('Reject invalid doc name', async () => {
      await expect(
        DOCUMENT_MANANGER.createDoc({
          doc: 'mydoc123!',
        }),
      ).rejects.toThrow('400')
    })

    test('Offline changes are synced to doc', async () => {
      const docResult = await DOCUMENT_MANANGER.createDoc()
      const key = await DOCUMENT_MANANGER.getClientToken(docResult, {})

      const doc = new Y.Doc()

      // Connect to the doc.
      const provider = createYjsProvider(doc, key, {})

      // Wait for the doc to sync.
      await new Promise<void>((resolve, reject) => {
        provider.on('synced', resolve)
        provider.on('syncing', reject)
      })

      // Disconnect.
      expect(provider.ws).not.toBeNull()
      provider.ws!.close()

      await new Promise<void>((resolve, reject) => {
        setTimeout(() => reject('Expected to disconnect.'), 1_000)
        provider.on('connection-close', () => {
          resolve()
        })
      })

      expect(provider.synced).toBe(false)

      // Modify the doc while offline.
      doc.getMap('test').set('foo', 'bar')

      // Reconnect to the doc.
      provider.connect()
      await new Promise<void>((resolve, reject) => {
        provider.on('status', (event: { status: string }) => {
          if (event.status === 'connected') {
            resolve()
          } else {
            reject(`Expected connected status, got ${event.status}`)
          }
        })
      })

      await new Promise<void>((resolve, reject) => {
        provider.on('sync', () => {
          resolve()
        })
        provider.on('syncing', reject)
      })
      expect(provider.synced).toBe(true)

      // Create a second doc.
      const doc2 = new Y.Doc()

      // Connect to the doc.
      const key2 = await DOCUMENT_MANANGER.getClientToken(docResult, {})
      const provider2 = createYjsProvider(doc2, key2, { WebSocketPolyfill: require('ws') })

      expect(doc2.getMap('test').get('foo')).toBeUndefined()

      // Wait for the doc to sync.
      await new Promise((resolve, reject) => {
        provider2.on('synced', resolve)
        provider2.on('syncing', reject)
      })

      // Ensure that the second doc received the changes.
      expect(doc2.getMap('test').get('foo')).toBe('bar')
    })

    if (configuration.useAuth) {
      test('Attempting to connect to a document without auth should fail', async () => {
        const docResult = await DOCUMENT_MANANGER.createDoc()
        const key = await DOCUMENT_MANANGER.getClientToken(docResult, {})

        expect(key.token).toBeDefined()
        delete key.token

        let ws = new WebSocket(`${key.url}/${key.doc}`)
        let result = new Promise<void>((resolve, reject) => {
          ws.addEventListener('open', () => {
            resolve()
          })
          ws.addEventListener('error', (e) => {
            reject(e.message)
          })
        })

        await expect(result).rejects.toContain('401')
      })
    }
  },
)
