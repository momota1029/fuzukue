const fs = require('node:fs');
const katex = require(process.argv[1]);
const [math, scripts] = JSON.parse(fs.readFileSync(0, 'utf8'));
for (const [tex, displayMode] of math) {
    try {
        katex.renderToString(tex, { displayMode, throwOnError: true });
    } catch (error) {
        console.log('BAD MATH', tex.slice(0, 80), error.message);
        process.exitCode = 1;
    }
}
for (const script of scripts) {
    try {
        new Function(script);
    } catch (error) {
        console.log('BAD SCRIPT', error.message);
        process.exitCode = 1;
    }
}
