import base64,json,sys,tempfile,unittest
from pathlib import Path
sys.path.insert(0,str(Path(__file__).resolve().parents[1]))
from cryptography.hazmat.primitives import hashes,serialization
from cryptography.hazmat.primitives.asymmetric import ec

class NativeProofTests(unittest.TestCase):
    def test_signed_refresh_is_bound_to_original_device_and_exact_native_message(self):
        import native_proof
        scratch=Path(__file__).resolve().parents[2]/'.superpowers/sdd/2026-10-12-trae-cn-unified-migration'
        with tempfile.TemporaryDirectory(dir=scratch) as tmp:
            root=Path(tmp)
            context=native_proof.prepare_device(root)
            info={'DevicePublicKey':context['public_key'],'DeviceID':context['device_id'],'MachineID':'fixture','PlatformCode':'IDE_PC'}
            native_proof.bind_device(root,'123',context['key_ref'],info)
            body=native_proof.refresh_request(root,'123','ono9krqynydwx5','fixture-refresh','3.4.2',timestamp=1234567,nonce='fixture-nonce')
            message='\n'.join(['POST','/trae/api/v3/oauth/ExchangeToken','ono9krqynydwx5','fixture-refresh','1234567','fixture-nonce']).encode()
            public=serialization.load_pem_public_key(context['public_key'].encode())
            public.verify(base64.b64decode(body['DeviceProof']['Signature']),message,ec.ECDSA(hashes.SHA256()))
            self.assertEqual(body['DeviceInfo']['DeviceID'],context['device_id'])
            self.assertEqual(body['IDEVersion'],'3.4.2')
            self.assertNotIn(b'PRIVATE KEY',b''.join(p.read_bytes() for p in root.rglob('*') if p.is_file()))
            with self.assertRaises(ValueError):native_proof.refresh_request(root,'other','ono9krqynydwx5','fixture-refresh','3.4.2')

    def test_wrong_device_public_key_cannot_be_bound(self):
        import native_proof
        scratch=Path(__file__).resolve().parents[2]/'.superpowers/sdd/2026-10-12-trae-cn-unified-migration'
        with tempfile.TemporaryDirectory(dir=scratch) as tmp:
            root=Path(tmp); context=native_proof.prepare_device(root)
            with self.assertRaises(ValueError):native_proof.bind_device(root,'123',context['key_ref'],{'DevicePublicKey':'wrong-key'})

    def test_export_import_keeps_protected_device_for_same_windows_user(self):
        import native_proof
        scratch=Path(__file__).resolve().parents[2]/'.superpowers/sdd/2026-10-12-trae-cn-unified-migration'
        with tempfile.TemporaryDirectory(dir=scratch) as tmp:
            root=Path(tmp); context=native_proof.prepare_device(root/'source')
            native_proof.bind_device(root/'source','123',context['key_ref'],{'DevicePublicKey':context['public_key'],'DeviceID':context['device_id']})
            exported=native_proof.export_device(root/'source','123')
            native_proof.import_device(root/'destination','123',exported)
            body=native_proof.refresh_request(root/'destination','123','ono9krqynydwx5','fixture-rt','3.4.2')
            self.assertEqual(body['DeviceInfo']['DevicePublicKey'],context['public_key'])
            self.assertNotIn('PRIVATE KEY',json.dumps(exported))
            with self.assertRaises(ValueError):native_proof.import_device(root/'destination','456',exported)

if __name__=='__main__':unittest.main()
