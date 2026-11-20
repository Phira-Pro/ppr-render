package org.pprrender.app;

import android.content.Context;
import android.view.KeyEvent;
import android.view.MotionEvent;
import android.view.Surface;
import android.view.SurfaceHolder;
import android.view.SurfaceView;
import android.view.View;

import quad_native.QuadNative;

/** 承载 Rust 渲染输出的 SurfaceView，并把触摸与按键转发给 libppr_render.so。 */
public class QuadSurface extends SurfaceView
        implements View.OnTouchListener, View.OnKeyListener, SurfaceHolder.Callback {

    public QuadSurface(Context context) {
        super(context);
        getHolder().addCallback(this);
        setFocusable(true);
        setFocusableInTouchMode(true);
        requestFocus();
        setOnTouchListener(this);
        setOnKeyListener(this);
    }

    @Override
    public void surfaceCreated(SurfaceHolder holder) {
        QuadNative.surfaceOnSurfaceCreated(holder.getSurface());
    }

    @Override
    public void surfaceDestroyed(SurfaceHolder holder) {
        QuadNative.surfaceOnSurfaceDestroyed(holder.getSurface());
    }

    @Override
    public void surfaceChanged(SurfaceHolder holder, int format, int width, int height) {
        QuadNative.surfaceOnSurfaceChanged(holder.getSurface(), width, height);
    }

    @Override
    public boolean onTouch(View v, MotionEvent event) {
        final int action = event.getActionMasked();
        switch (action) {
            case MotionEvent.ACTION_DOWN:
            case MotionEvent.ACTION_POINTER_DOWN:
                sendPointer(event, event.getActionIndex(), 2);
                break;
            case MotionEvent.ACTION_MOVE:
                for (int i = 0; i < event.getPointerCount(); i++) {
                    sendPointer(event, i, 0);
                }
                break;
            case MotionEvent.ACTION_UP:
            case MotionEvent.ACTION_POINTER_UP:
                sendPointer(event, event.getActionIndex(), 1);
                break;
            case MotionEvent.ACTION_CANCEL:
                for (int i = 0; i < event.getPointerCount(); i++) {
                    sendPointer(event, i, 3);
                }
                break;
            default:
                break;
        }
        return true;
    }

    private void sendPointer(MotionEvent event, int index, int phase) {
        QuadNative.surfaceOnTouch(
                event.getPointerId(index),
                phase,
                event.getX(index),
                event.getY(index),
                event.getEventTime());
    }

    @SuppressWarnings("deprecation")
    @Override
    public boolean onKey(View v, int keyCode, KeyEvent event) {
        if (event.getAction() == KeyEvent.ACTION_DOWN && keyCode != 0) {
            QuadNative.surfaceOnKeyDown(keyCode);
        }
        if (event.getAction() == KeyEvent.ACTION_UP && keyCode != 0) {
            QuadNative.surfaceOnKeyUp(keyCode);
        }
        if (event.getAction() == KeyEvent.ACTION_UP
                || event.getAction() == KeyEvent.ACTION_MULTIPLE) {
            int character = event.getUnicodeChar();
            if (character == 0) {
                String characters = event.getCharacters();
                if (characters != null && !characters.isEmpty()) {
                    character = characters.charAt(0);
                }
            }
            if (character != 0) {
                QuadNative.surfaceOnCharacter(character);
            }
        }
        return true;
    }
}
