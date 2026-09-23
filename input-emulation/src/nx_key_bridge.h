/*
 * Minimal macOS IOHID keyboard bridge.
 *
 * macOS only applies its own keyboard shortcuts when a modifier transition
 * arrives as real HID input: an NX_FLAGSCHANGED event carrying the physical
 * key code of the side that changed. The event is built from NXEventData, a
 * large SDK union, so the union stays in C and the Rust side only ever sees
 * fixed width integers and an opaque handle.
 *
 * IOHIDPostEvent is deprecated since macOS 11 and has no replacement; the
 * call is confined to this file so a future replacement stays a local change.
 */

#ifndef MOUSEHOP_NX_KEY_BRIDGE_H
#define MOUSEHOP_NX_KEY_BRIDGE_H

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* An open IOHIDSystem connection. */
typedef struct MousehopNxDriver MousehopNxDriver;

/*
 * Opens the IOHIDSystem service for parameter-connect posting.
 * Returns NULL when the service cannot be opened (the caller then uses the
 * CGEvent fallback).
 */
MousehopNxDriver *mousehop_nx_open(void);

/* Closes a driver returned by mousehop_nx_open. NULL is ignored. */
void mousehop_nx_close(MousehopNxDriver *driver);

/*
 * Posts an NX_FLAGSCHANGED event for key_code with the given complete
 * modifier flags, replacing the global modifier state.
 * Returns 0 (KERN_SUCCESS) or the raw kern_return_t.
 */
int32_t mousehop_nx_flags_changed(MousehopNxDriver *driver,
                                  uint16_t key_code,
                                  uint32_t flags);

/*
 * Posts an NX_KEYDOWN (down != 0) or NX_KEYUP event with no global flags, so
 * the modifier state published by mousehop_nx_flags_changed survives ordinary
 * keys.
 * Returns 0 (KERN_SUCCESS) or the raw kern_return_t.
 */
int32_t mousehop_nx_key(MousehopNxDriver *driver, uint16_t key_code, int32_t down);

#ifdef __cplusplus
}
#endif

#endif /* MOUSEHOP_NX_KEY_BRIDGE_H */
