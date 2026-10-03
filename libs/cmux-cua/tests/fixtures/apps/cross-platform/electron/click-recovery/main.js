const {app, BrowserWindow, ipcMain} = require('electron');
const fs = require('fs');
const path = require('path');
const countsFile = path.join(__dirname, 'counts.json');
let counts = {};
fs.writeFileSync(countsFile, JSON.stringify(counts));
ipcMain.on('hit', (_, id) => {
  counts[id] = (counts[id] || 0) + 1;
  fs.writeFileSync(countsFile, JSON.stringify(counts));
});
app.whenReady().then(() => {
  let win = new BrowserWindow({title:'CUA Electron Click Fixture', x:50, y:80,
    width:760, height:500, useContentSize:true,
    webPreferences:{nodeIntegration:true, contextIsolation:false}});
  win.loadFile(path.join(__dirname,'index.html'), {query:{requireFocus:process.env.CUA_CLICK_REQUIRE_FOCUS || '1'}});
});
app.on('window-all-closed', () => app.quit());
