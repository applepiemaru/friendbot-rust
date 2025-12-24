import fs from 'fs';
import CryptoJS from 'crypto-js';

const PARENT_DB_PATH = 'c:/Users/USER/Documents/bot discord/Evertext-Discord-Bot/data/db.json';
const TARGET_DB_PATH = 'db.json';
const SECRET_KEY = 'abcdefghijklmnopqrstuvwxyz123456';

const decrypt = (ciphertext) => {
    try {
        const bytes = CryptoJS.AES.decrypt(ciphertext, SECRET_KEY);
        return bytes.toString(CryptoJS.enc.Utf8);
    } catch (e) {
        console.error("Decryption error:", e.message);
        return null;
    }
};

try {
    if (!fs.existsSync(PARENT_DB_PATH)) {
        console.error(`Parent DB not found at ${PARENT_DB_PATH}`);
        process.exit(1);
    }

    const raw = fs.readFileSync(PARENT_DB_PATH, 'utf-8');
    const db = JSON.parse(raw);

    console.log(`Loaded DB with ${db.accounts.length} accounts.`);

    const newAccounts = db.accounts.map(acc => {
        // Create new account object without encryptedCode
        const { encryptedCode, ...rest } = acc;

        let plainCode = null;
        if (encryptedCode) {
            plainCode = decrypt(encryptedCode);
        }

        if (!plainCode) {
            console.warn(`[WARN] Could not decrypt code for ${acc.name}. Keeping null/empty.`);
            plainCode = "";
        }

        return {
            ...rest,
            code: plainCode,
            pingEnabled: false
        };
    });

    const newDb = {
        accounts: newAccounts,
        settings: db.settings
    };

    fs.writeFileSync(TARGET_DB_PATH, JSON.stringify(newDb, null, 2), 'utf-8');
    console.log(`[SUCCESS] Migrated DB saved to ${TARGET_DB_PATH}`);

} catch (e) {
    console.error("Migration failed:", e);
}
