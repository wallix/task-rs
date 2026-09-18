set -eu
rm -rf node_modules
mkdir -p node_modules/.bin node_modules/pkg/.github node_modules/@ws packages/lib
echo state > node_modules/.yarn-state.yml
echo pkg > node_modules/pkg/index.js
echo fund > node_modules/pkg/.github/FUNDING.yml
ln -s ../pkg/index.js node_modules/.bin/pkg
echo lib > packages/lib/index.js
ln -s ../../packages/lib node_modules/@ws/lib
