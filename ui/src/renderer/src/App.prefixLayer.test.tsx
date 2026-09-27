// @vitest-environment jsdom
import { act } from 'react'
import { afterEach, beforeEach, describe, expect, it } from 'vitest'
import { prefixLayer } from './prefixLayer'
import {
  type AppHarness,
  makeSession,
  makeWorkspace,
  renderReadyApp,
  resetHarness
} from './test/appTestHarness'
import { flushGhosttyAttach, ghosttyMock } from './test/ghosttySurfaceMock'

// End to end in jsdom: the pane's ghostty key handler (via the surface mock) claims the
// prefix, re-emits the next key on window, and App's dispatcher acts on it.

const A = '/tmp/ws-a'
const B = '/tmp/ws-b'

// Grids of unselected workspaces stay mounted (their terminals keep running) inside a
// wrapper with class `hidden`, so "visible" means present and not under such a wrapper.
function paneVisible(h: AppHarness, id: number): boolean {
  const pane = h.container.querySelector(`section.pane[data-panekey="${id}"]`)
  if (!pane) return false
  const host = pane.closest('[data-workspace]')
  return host === null || !host.classList.contains('hidden')
}

function focusPane(h: AppHarness, id: number): void {
  const pane = h.container.querySelector(`section.pane[data-panekey="${id}"]`)
  if (!(pane instanceof HTMLElement)) throw new Error(`pane ${id} not rendered`)
  act(() => {
    pane.dispatchEvent(new PointerEvent('pointerdown', { bubbles: true, cancelable: true }))
  })
  if (!h.container.querySelector('section.pane.focus')) throw new Error('pane did not focus')
}

// A key typed into the focused terminal: what ghostty would see first.
function termKey(init: KeyboardEventInit): boolean {
  let handled = true
  act(() => {
    handled = ghosttyMock.emitKey(new KeyboardEvent('keydown', { cancelable: true, ...init }))
  })
  return handled
}

async function settle(): Promise<void> {
  await act(async () => {
    await flushGhosttyAttach()
  })
}

async function wait(ms: number): Promise<void> {
  await act(async () => {
    await new Promise((r) => setTimeout(r, ms))
  })
}

describe('prefix layer from a focused terminal', () => {
  let h: AppHarness | null = null

  beforeEach(() => {
    resetHarness()
    localStorage.clear()
    prefixLayer.disarm()
  })
  afterEach(() => {
    prefixLayer.disarm()
    h?.unmount()
    h = null
  })

  async function boot(): Promise<AppHarness> {
    const harness = await renderReadyApp({
      workspaces: [makeWorkspace({ path: A, name: 'ws-a' }), makeWorkspace({ path: B, name: 'ws-b' })],
      sessions: [
        makeSession({ id: 1, project_dir: A, cwd: A, title: 'in-a' }),
        makeSession({ id: 2, project_dir: B, cwd: B, title: 'in-b' })
      ]
    })
    // Land on workspace A whichever one hello selected.
    if (!paneVisible(harness, 1)) {
      focusPane(harness, 2)
      termKey({ code: 'Space', key: ' ', ctrlKey: true })
      termKey({ code: 'KeyP', key: 'p' })
      await settle()
    }
    if (!paneVisible(harness, 1)) throw new Error('could not land on workspace A')
    return harness
  }

  it('a bare n stays with the terminal; prefix then n switches workspace', async () => {
    h = await boot()
    focusPane(h, 1)

    expect(termKey({ code: 'KeyN', key: 'n' })).toBe(true)
    expect(paneVisible(h, 1)).toBe(true)
    expect(paneVisible(h, 2)).toBe(false)

    expect(termKey({ code: 'Space', key: ' ', ctrlKey: true })).toBe(false)
    expect(prefixLayer.isArmed()).toBe(true)
    expect(termKey({ code: 'KeyN', key: 'n' })).toBe(false)
    await settle()
    expect(prefixLayer.isArmed()).toBe(false)
    expect(paneVisible(h, 2)).toBe(true)
    expect(paneVisible(h, 1)).toBe(false)
  })

  it('prefix then Tab returns to the last workspace', async () => {
    h = await boot()
    focusPane(h, 1)
    termKey({ code: 'Space', key: ' ', ctrlKey: true })
    termKey({ code: 'KeyN', key: 'n' })
    await settle()
    expect(paneVisible(h, 2)).toBe(true)

    focusPane(h, 2)
    termKey({ code: 'Space', key: ' ', ctrlKey: true })
    termKey({ code: 'Tab', key: 'Tab' })
    await settle()
    expect(paneVisible(h, 1)).toBe(true)
    expect(paneVisible(h, 2)).toBe(false)
  })

  it('a second Ctrl+Space reaches the terminal and disarms', async () => {
    h = await boot()
    focusPane(h, 1)
    expect(termKey({ code: 'Space', key: ' ', ctrlKey: true })).toBe(false)
    expect(termKey({ code: 'Space', key: ' ', ctrlKey: true })).toBe(true)
    expect(prefixLayer.isArmed()).toBe(false)
    expect(paneVisible(h, 1)).toBe(true)
  })

  it('Esc cancels the layer without reaching the pane, and the hint waits for a key', async () => {
    h = await boot()
    focusPane(h, 1)
    termKey({ code: 'Space', key: ' ', ctrlKey: true })
    expect(h.container.querySelector('[data-testid="prefix-hint"]')).toBeNull()
    await wait(320)
    expect(document.querySelector('[data-testid="prefix-hint"]')).not.toBeNull()
    await wait(900)
    expect(prefixLayer.isArmed()).toBe(true)
    expect(document.querySelector('[data-testid="prefix-hint"]')).not.toBeNull()

    expect(termKey({ code: 'Escape', key: 'Escape' })).toBe(false)
    expect(prefixLayer.isArmed()).toBe(false)
    await wait(0)
    expect(document.querySelector('[data-testid="prefix-hint"]')).toBeNull()
    expect(paneVisible(h, 1)).toBe(true)

    // and after the cancel, n is the terminal's again
    expect(termKey({ code: 'KeyN', key: 'n' })).toBe(true)
    expect(paneVisible(h, 1)).toBe(true)
  })

  it('prefix then Space opens the command palette; bare Ctrl+K stays with the terminal', async () => {
    h = await boot()
    focusPane(h, 1)
    expect(termKey({ code: 'KeyK', key: 'k', ctrlKey: true })).toBe(true)
    expect(document.querySelector('[data-testid="command-palette"]')).toBeNull()

    termKey({ code: 'Space', key: ' ', ctrlKey: true })
    expect(termKey({ code: 'Space', key: ' ' })).toBe(false)
    await settle()
    expect(document.querySelector('[data-testid="command-palette"]')).not.toBeNull()
  })

  it('the layer is forgotten after the safety timeout, so a stray prefix never eats a later key', async () => {
    h = await boot()
    focusPane(h, 1)
    termKey({ code: 'Space', key: ' ', ctrlKey: true })
    expect(prefixLayer.isArmed()).toBe(true)
    await wait(8100)
    expect(prefixLayer.isArmed()).toBe(false)
    expect(termKey({ code: 'KeyN', key: 'n' })).toBe(true)
    expect(paneVisible(h, 1)).toBe(true)
  }, 15000)
})
