(function() {
    'use strict';
    
    if (!window.chrome) window.chrome = {};
    
    if (!window.chrome.webview) {
        var _listeners = [];
        
        window.chrome.webview = {
            // JS -> Host: forward via Tauri invoke
            postMessage: function(data) {
                if (window.__TAURI__ && window.__TAURI__.core) {
                    window.__TAURI__.core.invoke('on_webview_message', {
                        payload: JSON.stringify(data)
                    });
                }
            },
            
            // Register listeners from host
            addEventListener: function(type, handler) {
                if (type === 'message') {
                    _listeners.push(handler);
                }
            },
            
            removeEventListener: function(type, handler) {
                if (type === 'message') {
                    _listeners = _listeners.filter(function(h) { return h !== handler; });
                }
            },
            
            // Host -> JS: called from Tauri via eval()
            __dispatchMessage: function(data) {
                var event = { data: data };
                for (var i = 0; i < _listeners.length; i++) {
                    try {
                        _listeners[i](event);
                    } catch(e) {
                        console.error('Polyfill dispatch error:', e);
                    }
                }
            }
        };
    }
})();
