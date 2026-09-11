(function () {
  const API_BASE_URL = 'https://api.easy-busy.cloud/chats/v1';
  const ASSETS_BASE_URL = 'https://chats.easy-busy.cloud/plugin';
  const pluginLoaderFilePath = 'https://chats.easy-busy.cloud/plugin/easy-busy-chats-plugin.umd.js';

  // CSS files to be injected during the build process
  const cssFiles = ["0.css"];
  const currentScript = document.currentScript || (function () {
    var scripts = document.getElementsByTagName('script');
    return scripts[scripts.length - 1];
  })();
  const loaderParams = new URLSearchParams(currentScript.src.split('?')[1] || '');
  const skipReact = loaderParams.get('skipreact') === '1';

  let reactIsReady = false;
  // Append a script to the document
  function appendScript(src, callback) {
    var script = document.createElement('script');
    script.src = src;
    script.onload = callback;
    document.head.appendChild(script);
  }

  // Load React and ReactDOM scripts
  if (skipReact) {
    reactIsReady = true;
  } else {
    appendScript('https://unpkg.com/react@18.3.1/umd/react.production.min.js', () => {
      appendScript('https://unpkg.com/react-dom@18.3.1/umd/react-dom.production.min.js', () => {
        reactIsReady = true;
      });
    });
  }

  function loadAllScripts(version) {
    appendScript(pluginLoaderFilePath + `?v=${version}`, () => {
      // Initialize the React app after all scripts are loaded
      if (window.chatsReadyCallback) {
        window.chatsReadyCallback();
      }
    });
  }

  // Append CSS files
  function appendCSS(href) {
    var link = document.createElement('link');
    link.rel = 'stylesheet';
    link.href = href;
    document.head.appendChild(link);
  }

  function getLatestVersion(callback) {
    var xhr = new XMLHttpRequest();
    xhr.open('GET', API_BASE_URL + '/plugin/version', true);
    xhr.onreadystatechange = function () {
      if (xhr.readyState === 4 && xhr.status === 200) {
        var response = JSON.parse(xhr.responseText);
        callback(response.version);
      }
    };
    xhr.send();
  }

  getLatestVersion((version) => {
    cssFiles.forEach(file => appendCSS(ASSETS_BASE_URL + '/' + file + `?v=${version}`));
    if (reactIsReady) {
      loadAllScripts(version);
    } else {
      let interval = setInterval(() => {
        if (!reactIsReady)
          return;
        loadAllScripts(version);
        clearInterval(interval);
      }, 50);
    }
  });

})();  