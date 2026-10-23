    (async () => {
      const status = document.getElementById('drive-status');
      if (new URLSearchParams(location.search).get('googledrive') === 'error') {
        status.textContent = 'Connection could not be verified. Please try connecting again.';
        return;
      }
      try {
        const response = await fetch('/api/oauth/googledrive/status');
        if (!response.ok) throw new Error('status failed');
        const data = await response.json();
        status.textContent = data.isConnected
          ? 'Connected: ' + data.accounts.map(a => a.email || a.entityId).join(', ')
          : 'No Drive account connected yet.';
      } catch {
        status.textContent = 'Unable to check connection status.';
      }
    })();
