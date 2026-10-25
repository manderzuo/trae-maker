"""Native P-256 device proof. Private keys stay in current-user DPAPI files.

stdin/stdout is a private parent-child pipe; never pass refresh tokens as CLI args.
"""
import base64,hashlib,json,os,re,secrets,sys,time
from pathlib import Path
from cryptography.hazmat.primitives import hashes,serialization
from cryptography.hazmat.primitives.asymmetric import ec
import win32crypt

PATH='/trae/api/v3/oauth/ExchangeToken'

def _write(path,value):
    path.parent.mkdir(parents=True,exist_ok=True)
    temp=path.with_name(path.name+'.'+secrets.token_hex(8)+'.tmp')
    try:
        temp.write_text(json.dumps(value),encoding='utf-8')
        os.replace(temp,path)
    finally:
        if temp.exists():temp.unlink()

def _uid(uid):
    if not re.fullmatch(r'[A-Za-z0-9_-]{1,64}',uid):raise ValueError('Invalid native account UID')
    return uid

def _keys(root):return Path(root)/'conf/native_oauth_keys'

def _store_key(root,pair):
    ref=hashlib.sha256(pair['public_key'].encode()).hexdigest()
    ciphertext=win32crypt.CryptProtectData(json.dumps(pair).encode(),None,None,None,None,1)
    _write(_keys(root)/(ref+'.json'),{'protected':base64.b64encode(ciphertext).decode()})
    return ref

def _load_key(root,ref):
    if not re.fullmatch(r'[a-f0-9]{64}',ref):raise ValueError('Invalid native device key reference')
    stored=json.loads((_keys(root)/(ref+'.json')).read_text(encoding='utf-8'))
    plain=win32crypt.CryptUnprotectData(base64.b64decode(stored['protected']),None,None,None,0)[1]
    pair=json.loads(plain)
    if hashlib.sha256(pair['public_key'].encode()).hexdigest()!=ref:raise ValueError('Native device key mismatch')
    return pair

def prepare_device(root):
    default=Path(root)/'conf/native_oauth_default.json'
    if default.exists():
        ref=json.loads(default.read_text(encoding='utf-8'))['key_ref'];pair=_load_key(root,ref)
    else:
        key=ec.generate_private_key(ec.SECP256R1())
        pair={'private_key':key.private_bytes(serialization.Encoding.PEM,serialization.PrivateFormat.PKCS8,serialization.NoEncryption()).decode(),
              'public_key':key.public_key().public_bytes(serialization.Encoding.PEM,serialization.PublicFormat.SubjectPublicKeyInfo).decode(),
              'device_id':'aha-'+secrets.token_hex(16)}
        ref=_store_key(root,pair);_write(default,{'key_ref':ref})
    return {'key_ref':ref,'public_key':pair['public_key'],'device_id':pair['device_id']}

def bind_device(root,uid,ref,device_info):
    pair=_load_key(root,ref)
    if device_info.get('DevicePublicKey')!=pair['public_key']:raise ValueError('Native device public key mismatch')
    allowed=['DeviceID','MachineID','PlatformCode','DeviceType','DeviceName','DeviceModel','ClientVersion','DevicePublicKey','DeviceBrand','DeviceCPU','OSInfo','OSVersion']
    info={k:v for k,v in device_info.items() if k in allowed and isinstance(v,str) and len(v)<=16384}
    _write(Path(root)/'conf/native_oauth_bindings'/(_uid(uid)+'.json'),{'key_ref':ref,'device_info':info})

def captured_device(root,uid,device_info):
    """Retain request-proven device context even if native key storage arrives later."""
    if not isinstance(device_info,dict) or not device_info.get('DevicePublicKey'):return
    path=Path(root)/'conf/native_oauth_bindings'/(_uid(uid)+'.json')
    if path.exists():
        old=json.loads(path.read_text(encoding='utf-8'))
        if old.get('device_info',{}).get('DevicePublicKey')==device_info['DevicePublicKey']:return
    # Only a validated ExchangeToken response calls this; accept the public
    # context, never private material from an HTTP request.
    allowed=['DeviceID','MachineID','PlatformCode','DeviceType','DeviceName','DeviceModel','ClientVersion','DevicePublicKey','DeviceBrand','DeviceCPU','OSInfo','OSVersion']
    info={k:v for k,v in device_info.items() if k in allowed and isinstance(v,str) and len(v)<=16384}
    if not info.get('DevicePublicKey'):return
    _write(path,{'device_info':info})

DEVICE_KEY_PREFIX='iCubeAuthInfo://icube-dc:'

def _native_key_pairs(root,uid):
    """Yield (pair, device_id) from client profiles holding a device key pair for uid.

    TRAE CN 3.4 stores it as `iCubeAuthInfo://icube-dc:<deviceId>`; older clients used a
    `*:<uid>` suffix. Device-level entries are only accepted while the profile's own
    CloudIDE session belongs to uid, so a key is never borrowed for another account.
    """
    import native_client
    p=native_client.resolve()
    paths=[Path(root)/'data/profiles_trae'/uid/'User/globalStorage/storage.json',
           Path(root)/'data/profiles'/uid/'User/globalStorage/storage.json',
           Path(p['data_dir'])/'User/globalStorage/storage.json']
    for path in paths:
        try:
            storage=json.loads(path.read_text(encoding='utf-8-sig'))
            auth=native_client.decode_tc(storage.get('iCubeAuthInfo://icube.cloudide',''))
            same_uid=isinstance(auth,dict) and str(auth.get('userId'))==uid
            if not same_uid:continue
            for name,value in storage.items():
                if not isinstance(value,str):continue
                if name.endswith(':'+uid):device_id=''
                elif name.startswith(DEVICE_KEY_PREFIX):device_id=name[len(DEVICE_KEY_PREFIX):]
                else:continue
                try:pair=native_client.decode_tc(value)
                except Exception:continue
                if not isinstance(pair,dict) or not pair.get('privateKeyPEM') or not pair.get('publicKeyPEM'):continue
                yield pair,device_id
        except (OSError,ValueError,TypeError):continue

def _import_native_key(root,uid,expected_public,refresh_token):
    for pair,device_id in _native_key_pairs(root,uid):
        matched = pair['publicKeyPEM']==expected_public if expected_public else True
        if matched:
            return _store_key(root,{'private_key':pair['privateKeyPEM'],'public_key':pair['publicKeyPEM'],'device_id':device_id})
    raise ValueError('Native refresh device key unavailable; authorize this account again')

def _bootstrap_binding(root,uid):
    """Create the binding from the key pair already present in the client profile.

    Only public context is persisted here; the private key stays in the DPAPI store via
    `_store_key`, mirroring what `captured_device` does after a validated exchange.
    """
    for pair,device_id in _native_key_pairs(root,uid):
        ref=_store_key(root,{'private_key':pair['privateKeyPEM'],'public_key':pair['publicKeyPEM'],'device_id':device_id})
        info={'DevicePublicKey':pair['publicKeyPEM']}
        if device_id:info['DeviceID']=device_id
        bind_device(root,uid,ref,info)
        return True
    return False

def refresh_request(root,uid,client_id,refresh_token,version,timestamp=None,nonce=None):
    path=Path(root)/'conf/native_oauth_bindings'/(_uid(uid)+'.json')
    if not path.exists() and not _bootstrap_binding(root,uid):
        raise ValueError('Native refresh device binding unavailable; authorize this account again')
    binding=json.loads(path.read_text(encoding='utf-8'))
    info=binding['device_info']
    ref=binding.get('key_ref') or _import_native_key(root,uid,info.get('DevicePublicKey'),refresh_token)
    pair=_load_key(root,ref)
    if info.get('DevicePublicKey')!=pair['public_key']:raise ValueError('Native refresh device binding mismatch')
    if not binding.get('key_ref'):bind_device(root,uid,ref,info)
    stamp=int(time.time()) if timestamp is None else timestamp
    rand=secrets.token_hex(16) if nonce is None else nonce
    message='\n'.join(['POST',PATH,client_id,refresh_token,str(stamp),rand]).encode()
    key=serialization.load_pem_private_key(pair['private_key'].encode(),None)
    signature=key.sign(message,ec.ECDSA(hashes.SHA256()))
    info={**info,'ClientVersion':version}
    return {'ClientID':client_id,'ClientSecret':'','RefreshToken':refresh_token,'DeviceInfo':info,
            'DeviceProof':{'Signature':base64.b64encode(signature).decode(),'Timestamp':stamp,'Nonce':rand},'IDEVersion':version}

def export_device(root,uid):
    path=Path(root)/'conf/native_oauth_bindings'/(_uid(uid)+'.json')
    if not path.exists():return None
    binding=json.loads(path.read_text(encoding='utf-8'))
    if not binding.get('key_ref'):raise ValueError('Native device binding not yet captured completely')
    _load_key(root,binding['key_ref'])
    protected=json.loads((_keys(root)/(binding['key_ref']+'.json')).read_text(encoding='utf-8'))
    return {'uid':uid,'binding':binding,'protected_key':protected}

def import_device(root,uid,record):
    if record.get('uid')!=_uid(uid):raise ValueError('Imported native device UID mismatch')
    binding=record['binding'];ref=binding['key_ref']
    if not re.fullmatch(r'[a-f0-9]{64}',ref):raise ValueError('Invalid native device reference')
    protected=record['protected_key']
    plain=win32crypt.CryptUnprotectData(base64.b64decode(protected['protected']),None,None,None,0)[1]
    pair=json.loads(plain)
    if hashlib.sha256(pair['public_key'].encode()).hexdigest()!=ref or binding['device_info'].get('DevicePublicKey')!=pair['public_key']:
        raise ValueError('Imported native device key mismatch')
    _write(_keys(root)/(ref+'.json'),protected)
    bind_device(root,uid,ref,binding['device_info'])

def main():
    try:
        request=json.load(sys.stdin);root=request['root'];action=request['action']
        if action=='prepare':result=prepare_device(root)
        elif action=='bind':bind_device(root,request['uid'],request['key_ref'],request['device_info']);result={}
        elif action=='refresh':result=refresh_request(root,request['uid'],request['client_id'],request['refresh_token'],request['version'])
        elif action=='keypair':
            binding=json.loads((Path(root)/'conf/native_oauth_bindings'/(_uid(request['uid'])+'.json')).read_text(encoding='utf-8'))
            pair=_load_key(root,binding['key_ref'])
            result={'privateKeyPEM':pair['private_key'],'publicKeyPEM':pair['public_key']}
        elif action=='export':result=export_device(root,request['uid'])
        elif action=='import':import_device(root,request['uid'],request['device']);result={}
        else:raise ValueError('Unknown native proof action')
        print(json.dumps(result))
    except Exception:
        # Exceptions can contain credentials supplied by serialization/crypto libraries.
        print(json.dumps({'error':'原生设备凭据不可用，请重新进行原生授权；现有登录凭据已保留'}))
        return 1
    return 0

if __name__=='__main__':raise SystemExit(main())
